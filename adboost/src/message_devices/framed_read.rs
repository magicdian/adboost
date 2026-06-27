//! Cancel-safe framed-read buffering shared by every byte-stream transport.
//!
//! # The invariant this module owns
//!
//! Every [`ADBMessageTransport`] frames a byte stream as a fixed 24-byte header
//! followed by a `data_length`-byte payload, and multiplexes many logical
//! sessions over one physical connection. A single mis-aligned frame is therefore
//! catastrophic: it does not drop one frame, it desyncs the shared stream and
//! tears down every session on the connection.
//!
//! The load-bearing rule that keeps the stream aligned is:
//!
//! > **A read timeout must never be observed mid-frame.** The transport-neutral
//! > [`RustADBError::ReadTimeout`] may be returned only when **zero** bytes of the
//! > current frame have been *consumed*; bytes already pulled off the wire are
//! > always retained across calls.
//!
//! Historically each transport re-implemented this and the TCP path got it wrong:
//! `tokio::time::timeout(reader.read_exact(buf))` drops the (non-cancel-safe)
//! `read_exact` future on timeout, losing the bytes already moved into `buf`, so
//! the next read starts mid-frame → an illegal command word → a fatal
//! [`RustADBError::ConversionError`] → the whole multiplexed connection dies. The
//! USB path avoided this only by accident of a persistent `read_residual` buffer
//! plus `nusb`'s atomic transfer cancellation.
//!
//! [`FrameReadBuffer`] makes the invariant explicit and shared. It is **sans-io**:
//! it owns a persistent byte buffer and a pure [`FrameReadBuffer::try_parse`] step;
//! the per-transport async code only has to push freshly-read bytes into it and
//! retry. Because the buffer lives on the transport (not on the call stack), a
//! cancelled chunk read loses nothing — every received byte is already buffered.
//!
//! # The feed-layer obligation (cancel-safety is only as strong as the feed)
//!
//! The buffer's losslessness holds **only if the feed layer pushes every byte a
//! transfer actually delivered, including a transfer that timed out.** A chunk
//! read that is cancelled to enforce a per-transfer timeout can still complete
//! with real bytes (the transfer landed in the same instant the timer fired). The
//! feed layer MUST push those bytes into [`push`](FrameReadBuffer::push) *before*
//! surfacing the timeout — it MUST NOT return [`RustADBError::ReadTimeout`] while
//! discarding a timed-out completion's payload. Doing so drops bytes that are
//! genuinely on the wire, so the next read resumes at a shifted offset and
//! [`try_parse`](FrameReadBuffer::try_parse) decodes a header out of mid-payload
//! bytes → a fatal [`RustADBError::ConversionError`] that tears the whole
//! multiplexed connection down. The USB transport hit exactly this: it ran its
//! status→error mapping before reading the drained byte count and dropped the
//! raced bytes, manifesting as an intermittent connection-fatal desync under
//! sustained shell-v2 PTY output. The fix classifies a completion on
//! `(status, byte_count)` together so bytes are always salvaged first (see
//! `usb_transport::classify_read_completion`). A `ReadTimeout` is therefore only
//! ever correct for a timed-out completion that delivered **zero** bytes.
//!
//! [`ADBMessageTransport`]: crate::message_devices::adb_message_transport::ADBMessageTransport

use crate::{
    Result, RustADBError,
    message_devices::adb_transport_message::{
        ADBTransportMessage, ADBTransportMessageHeader, HEADER_LENGTH, MAX_PAYLOAD,
        payload_len_within_bound,
    },
};

/// A persistent, cancel-safe accumulation buffer for one read direction of a
/// framed transport.
///
/// Push freshly-read bytes with [`push`](Self::push); attempt to extract a whole
/// frame with [`try_parse`](Self::try_parse). Bytes are consumed from the buffer
/// **only** when an entire frame (header + payload) is present, so a read that is
/// cancelled (timed out) part-way through a frame leaves the already-received
/// bytes in place for the next call — the stream can never desync.
#[derive(Debug, Default)]
pub(crate) struct FrameReadBuffer {
    /// Bytes received off the wire but not yet consumed as a complete frame.
    /// Holds at most one partial frame plus any over-read into the next frame.
    buf: Vec<u8>,
}

impl FrameReadBuffer {
    /// A fresh, empty buffer.
    pub(crate) fn new() -> Self {
        Self { buf: Vec::new() }
    }

    /// Discard all buffered bytes.
    ///
    /// Used when the underlying stream is structurally reset (e.g. a USB
    /// (re)connect must not let a stale CLSE/WRTE left over from a prior
    /// connection's framed stream desync the fresh CNXN handshake). The TCP
    /// transport instead builds a fresh reader on its TLS upgrade, so this is only
    /// reached on the USB path.
    #[cfg(feature = "usb")]
    pub(crate) fn clear(&mut self) {
        self.buf.clear();
    }

    /// Append freshly-read bytes to the buffer.
    pub(crate) fn push(&mut self, data: &[u8]) {
        self.buf.extend_from_slice(data);
    }

    /// Try to extract one complete frame from the buffered bytes.
    ///
    /// Returns:
    /// - `Ok(Some(message))` — a full frame was present; its bytes (and only its
    ///   bytes) have been consumed from the buffer, leaving any over-read bytes
    ///   for the next frame.
    /// - `Ok(None)` — not enough bytes buffered yet for a complete frame; the
    ///   caller should read more and retry. Nothing is consumed.
    /// - `Err(_)` — the header decoded to something invalid (bad command word →
    ///   [`RustADBError::ConversionError`], oversize `data_length`, or a magic
    ///   mismatch). These are genuine protocol errors, not desyncs.
    ///
    /// The `data_length` is bounded against [`MAX_PAYLOAD`] **before** the total
    /// frame size is used to decide completeness, so a hostile/corrupt header can
    /// never make the buffer wait for (or allocate) a ~4 GiB frame.
    pub(crate) fn try_parse(&mut self) -> Result<Option<ADBTransportMessage>> {
        // Need a full header before we can know the frame length.
        if self.buf.len() < HEADER_LENGTH {
            return Ok(None);
        }

        // Peek (do NOT consume) the 24-byte header so a still-incomplete payload
        // leaves the header in place for the next attempt.
        let mut header_bytes = [0_u8; HEADER_LENGTH];
        header_bytes.copy_from_slice(&self.buf[..HEADER_LENGTH]);
        let header = ADBTransportMessageHeader::try_from(header_bytes)?;

        // Bound the wire data_length BEFORE using it as a length (AOSP
        // check_header clause). A corrupt 24-byte header could otherwise drive an
        // unbounded wait / allocation.
        if !payload_len_within_bound(header.data_length()) {
            return Err(RustADBError::ADBRequestFailed(format!(
                "frame data_length {} exceeds MAX_PAYLOAD {MAX_PAYLOAD}",
                header.data_length()
            )));
        }

        // data_length is now proven <= MAX_PAYLOAD, so this addition cannot
        // overflow usize on any supported target.
        let frame_len = HEADER_LENGTH + header.data_length() as usize;
        if self.buf.len() < frame_len {
            // Header is in; payload not fully arrived yet. Keep everything.
            return Ok(None);
        }

        // A whole frame is present: consume exactly its bytes, retaining any
        // over-read tail (the start of the next frame) in the buffer.
        let payload = self.buf[HEADER_LENGTH..frame_len].to_vec();
        self.buf.drain(..frame_len);

        let message = ADBTransportMessage::from_header_and_payload(header, payload);

        // Magic-only integrity (AOSP-faithful; runs for every frame). Note the
        // frame has already been fully consumed off the buffer at this point, so
        // an integrity failure is frame-aligned and recoverable by the caller
        // (mirrors the persistent reader's InvalidIntegrity skip path).
        if !message.check_message_integrity() {
            return Err(RustADBError::InvalidIntegrity(
                ADBTransportMessageHeader::compute_magic(message.header().command()),
                message.header().magic(),
            ));
        }

        Ok(Some(message))
    }
}

#[cfg(test)]
mod tests {
    use super::FrameReadBuffer;
    use crate::{
        RustADBError,
        message_devices::{
            adb_transport_message::{ADBTransportMessage, HEADER_LENGTH},
            message_commands::MessageCommand,
        },
    };

    /// Serialize a complete frame (header + payload) the way it appears on the wire.
    fn frame_bytes(command: MessageCommand, arg0: u32, arg1: u32, payload: &[u8]) -> Vec<u8> {
        let msg =
            ADBTransportMessage::try_new(command, arg0, arg1, payload).expect("build wire message");
        let mut bytes = msg.header().as_bytes();
        bytes.extend_from_slice(payload);
        bytes
    }

    #[test]
    fn empty_buffer_yields_none() {
        let mut buf = FrameReadBuffer::new();
        assert!(
            buf.try_parse().expect("empty parse ok").is_none(),
            "no bytes buffered must parse to None, not an error"
        );
    }

    #[test]
    fn partial_header_yields_none_and_consumes_nothing() {
        let frame = frame_bytes(MessageCommand::Okay, 1, 2, b"");
        let mut buf = FrameReadBuffer::new();
        // Feed all but the last header byte.
        buf.push(&frame[..HEADER_LENGTH - 1]);
        assert!(
            buf.try_parse().expect("partial header ok").is_none(),
            "an incomplete header must parse to None"
        );
        // Now complete it.
        buf.push(&frame[HEADER_LENGTH - 1..]);
        let msg = buf
            .try_parse()
            .expect("complete frame ok")
            .expect("a complete frame is available");
        assert_eq!(
            msg.header().command(),
            MessageCommand::Okay,
            "the assembled frame must decode to the pushed command"
        );
    }

    #[test]
    fn header_present_payload_pending_yields_none() {
        let frame = frame_bytes(MessageCommand::Write, 9, 9, b"hello world");
        let mut buf = FrameReadBuffer::new();
        // Full header + only part of the payload.
        buf.push(&frame[..HEADER_LENGTH + 4]);
        assert!(
            buf.try_parse().expect("payload-pending ok").is_none(),
            "a full header with an incomplete payload must parse to None (no desync)"
        );
        // Deliver the rest.
        buf.push(&frame[HEADER_LENGTH + 4..]);
        let msg = buf
            .try_parse()
            .expect("now complete")
            .expect("frame available");
        assert_eq!(
            msg.payload().as_slice(),
            b"hello world",
            "the payload must be reassembled intact across the split"
        );
    }

    #[test]
    fn byte_at_a_time_delivery_stays_aligned() {
        // The cancel-safety property in pure form: a frame delivered one byte per
        // push (each push modeling a chunk read that could have been preceded by a
        // timeout) must reassemble intact, and try_parse must consume nothing until
        // the whole frame is present.
        let frame = frame_bytes(MessageCommand::Write, 0, 0, b"ifconfig-ish payload");
        let mut buf = FrameReadBuffer::new();
        for (i, byte) in frame.iter().enumerate() {
            buf.push(std::slice::from_ref(byte));
            let parsed = buf.try_parse().expect("incremental parse ok");
            if i + 1 < frame.len() {
                assert!(
                    parsed.is_none(),
                    "frame must not be emitted before all {} bytes arrive (had {})",
                    frame.len(),
                    i + 1
                );
            } else {
                assert!(parsed.is_some(), "the final byte must complete the frame");
            }
        }
    }

    #[test]
    fn over_read_tail_is_retained_for_next_frame() {
        // One push carrying frame A in full plus the first bytes of frame B must
        // yield A and keep B's prefix buffered (the over-read carry that USB's
        // read_residual used to handle).
        let frame_a = frame_bytes(MessageCommand::Okay, 1, 1, b"");
        let frame_b = frame_bytes(MessageCommand::Write, 2, 2, b"second");
        let mut buf = FrameReadBuffer::new();
        buf.push(&frame_a);
        buf.push(&frame_b[..5]); // partial B coalesced into the same read

        let a = buf.try_parse().expect("A parses").expect("A is complete");
        assert_eq!(
            a.header().command(),
            MessageCommand::Okay,
            "first frame is A"
        );
        assert!(
            buf.try_parse().expect("B pending ok").is_none(),
            "B's prefix must be retained but not yet complete"
        );

        buf.push(&frame_b[5..]);
        let b = buf.try_parse().expect("B parses").expect("B now complete");
        assert_eq!(
            b.payload().as_slice(),
            b"second",
            "the over-read tail must form the next frame intact"
        );
    }

    #[test]
    fn two_frames_in_one_push_parse_sequentially() {
        let frame_a = frame_bytes(MessageCommand::Okay, 1, 1, b"");
        let frame_b = frame_bytes(MessageCommand::Write, 2, 2, b"xyz");
        let mut combined = frame_a.clone();
        combined.extend_from_slice(&frame_b);

        let mut buf = FrameReadBuffer::new();
        buf.push(&combined);

        let a = buf.try_parse().expect("A ok").expect("A present");
        assert_eq!(
            a.header().command(),
            MessageCommand::Okay,
            "first parse is A"
        );
        let b = buf.try_parse().expect("B ok").expect("B present");
        assert_eq!(b.payload().as_slice(), b"xyz", "second parse is B");
        assert!(
            buf.try_parse().expect("drained ok").is_none(),
            "buffer is fully drained after both frames"
        );
    }

    #[cfg(feature = "usb")]
    #[test]
    fn clear_discards_buffered_bytes() {
        let frame = frame_bytes(MessageCommand::Write, 0, 0, b"partial");
        let mut buf = FrameReadBuffer::new();
        buf.push(&frame[..HEADER_LENGTH + 2]);
        buf.clear();
        assert!(
            buf.try_parse().expect("post-clear ok").is_none(),
            "clear() must drop buffered bytes so no stale frame is parsed"
        );
    }

    #[test]
    fn illegal_command_word_is_conversion_error() {
        // A 24-byte header whose command word is not a known MessageCommand must
        // surface as ConversionError (the fatal-desync signal), not None.
        let mut buf = FrameReadBuffer::new();
        let mut bogus = [0_u8; HEADER_LENGTH];
        bogus[0..4].copy_from_slice(&0xDEAD_BEEF_u32.to_le_bytes());
        buf.push(&bogus);
        assert!(
            matches!(buf.try_parse(), Err(RustADBError::ConversionError)),
            "an unknown command word must decode to ConversionError"
        );
    }
}
