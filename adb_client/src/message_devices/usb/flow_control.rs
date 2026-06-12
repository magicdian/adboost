//! Sans-io `delayed_ack` flow-control state machine.
//!
//! This is the I/O-free heart of Ask #1 (windowed flow control). It models the
//! AOSP `delayed_ack` send/receive window semantics as a pure state machine so
//! it can be unit-tested without any USB hardware (D1). The `persistent.rs`
//! read/write halves drive it; this module never touches a socket.
//!
//! Wire semantics (verified against AOSP, see research/07):
//! - The OKAY byte count rides in the OKAY **payload** as a 4-byte little-endian
//!   signed `i32` — NOT in `arg0`/`arg1` (those stay local/remote socket ids).
//!   A classic-mode OKAY carries an **empty** payload.
//! - The value is a **delta** (bytes the receiver just drained), and is signed —
//!   it MAY be negative (reserved for future preemptive backpressure). The
//!   sender accumulates: `available_bytes += delta`.
//! - The initial window is 32 MiB ([`INITIAL_DELAYED_ACK_BYTES`]), granted
//!   per-stream: the opener puts it in OPEN `arg1` and sets its OWN send window
//!   to 0; the responder grants its own window via the first OKAY payload.
//! - Per-WRTE chunk is clamped to [`MAX_PAYLOAD`] (1 MiB), decoupled from the
//!   in-flight window.
//! - Overflow is pure self-throttling backpressure: the window may go `<= 0`
//!   (even slightly negative); the sender then waits for an OKAY to credit it
//!   back. There is no stream close on overflow.

/// Maximum bytes in a single WRTE payload (AOSP `MAX_PAYLOAD`, 1 MiB). The
/// per-chunk cap, decoupled from the in-flight window size.
///
/// Re-exported from the always-compiled `adb_transport_message` module so the
/// USB chunk clamp and both transport read-path bound checks share one
/// definition (the TCP read path cannot see the `usb` module without the `usb`
/// feature).
pub use crate::message_devices::adb_transport_message::MAX_PAYLOAD;

/// Initial in-flight window granted per stream when `delayed_ack` is negotiated
/// (AOSP `INITIAL_DELAYED_ACK_BYTES`, 32 MiB).
pub const INITIAL_DELAYED_ACK_BYTES: i64 = 32 * 1024 * 1024;

/// Per-session send-window accounting for `delayed_ack` windowed flow control.
///
/// Mirrors AOSP's `asocket::available_send_bytes`: a single signed running
/// window. `None` means `delayed_ack` was NOT negotiated → classic strict
/// stop-and-wait (one WRTE per OKAY, empty-payload OKAYs). `Some` means windowed
/// mode is active and `available_bytes` is the remaining in-flight credit.
///
/// The load-bearing field is the single signed `available_bytes` accumulator
/// (mirroring AOSP, which keeps no separate cumulative sent/acked counters).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct FlowControl {
    /// `None` = classic stop-and-wait; `Some(window)` = windowed, remaining
    /// in-flight credit in bytes (signed, may go `<= 0`).
    available_bytes: Option<i64>,
}

impl FlowControl {
    /// Construct a windowed flow controller with the given initial send window.
    ///
    /// As the OPENER (host `open_session`), AOSP sets its OWN send window to 0
    /// and waits for the device's first OKAY (carrying the device's grant) to
    /// credit it — so pass `0` here for the opener and let `on_okay_payload`
    /// raise the window. See research/07 Q2.
    #[must_use]
    pub const fn new_windowed(initial_window: i64) -> Self {
        Self {
            available_bytes: Some(initial_window),
        }
    }

    /// Construct a classic (non-`delayed_ack`) flow controller: strict
    /// stop-and-wait, empty-payload OKAYs.
    #[must_use]
    pub const fn new_classic() -> Self {
        Self {
            available_bytes: None,
        }
    }

    /// Whether windowed (`delayed_ack`) mode is active.
    #[must_use]
    pub const fn is_windowed(&self) -> bool {
        self.available_bytes.is_some()
    }

    /// Remaining in-flight send credit (windowed mode only).
    #[must_use]
    pub const fn available_bytes(&self) -> Option<i64> {
        self.available_bytes
    }

    /// Whether the sender may emit `n` more bytes right now.
    ///
    /// In classic mode this returns `true` only when nothing is in flight (the
    /// stop-and-wait rendezvous is enforced by the caller blocking on the ack
    /// channel after each chunk). In windowed mode the sender may keep emitting
    /// while the window is `> 0`; AOSP allows one final chunk to drive the
    /// window `<= 0`, so we permit a send as long as credit remains positive.
    #[must_use]
    pub const fn can_send(&self) -> bool {
        match self.available_bytes {
            // Windowed: send while credit remains. One in-flight chunk may push
            // it <= 0 (AOSP behavior); the next send then waits for an OKAY.
            Some(window) => window > 0,
            // Classic: stop-and-wait is enforced by the caller per chunk.
            None => true,
        }
    }

    /// Debit the window after emitting a WRTE of `n` bytes.
    ///
    /// No-op accounting in classic mode (window is `None`). In windowed mode the
    /// window is signed, so a debit driving it negative is expected and safe
    /// (saturating to avoid `i64` underflow on absurd inputs).
    pub fn record_sent(&mut self, n: usize) {
        if let Some(window) = self.available_bytes.as_mut() {
            *window = window.saturating_sub(i64::try_from(n).unwrap_or(i64::MAX));
        }
    }

    /// Apply an OKAY payload to the send window.
    ///
    /// Parses the optional 4-byte LE signed `i32` delta and accumulates it:
    /// `available_bytes += delta`. An empty payload is a classic/no-op OKAY (the
    /// rendezvous signal). A payload whose length is neither 0 nor 4 is rejected
    /// (returns `false`) and ignored, matching AOSP's "drop the packet" behavior;
    /// the caller decides whether to log.
    ///
    /// Returns `true` if the payload was accepted (including the empty/classic
    /// case), `false` if it was malformed.
    pub fn on_okay_payload(&mut self, payload: &[u8]) -> bool {
        match payload.len() {
            0 => true, // classic / no-op OKAY rendezvous
            4 => {
                // Signed i32, little-endian. May be negative.
                let bytes: [u8; 4] = match payload.try_into() {
                    Ok(b) => b,
                    Err(_) => return false,
                };
                let delta = i32::from_le_bytes(bytes);
                self.apply_delta(i64::from(delta));
                true
            }
            _ => false,
        }
    }

    /// Apply a raw signed delta to the window (used by `on_okay_payload`; also
    /// directly testable). No-op in classic mode.
    pub fn apply_delta(&mut self, delta: i64) {
        if let Some(window) = self.available_bytes.as_mut() {
            *window = window.saturating_add(delta);
        }
    }
}

/// Serialize a receive-side OKAY delta (bytes just delivered to the consumer)
/// into the OKAY payload for windowed mode, or an empty payload for classic.
///
/// In windowed mode the receiver eagerly emits `OKAY(payload = bytes_delivered
/// as i32 LE)` each time it hands data to the consumer (research/07 step 7); a
/// 0-delta is acceptable. In classic mode the OKAY carries an empty payload.
/// `bytes` is clamped to `i32::MAX` (a single delivered chunk never exceeds
/// `MAX_PAYLOAD` = 1 MiB, so the clamp is purely defensive).
#[must_use]
pub fn encode_okay_payload(windowed: bool, bytes: usize) -> Vec<u8> {
    if windowed {
        let delta = i32::try_from(bytes).unwrap_or(i32::MAX);
        delta.to_le_bytes().to_vec()
    } else {
        Vec::new()
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn classic_mode_has_no_window() {
        let fc = FlowControl::new_classic();
        assert!(!fc.is_windowed(), "classic mode must not be windowed");
        assert_eq!(
            fc.available_bytes(),
            None,
            "classic mode has no window value"
        );
        assert!(
            fc.can_send(),
            "classic mode always permits a send (caller enforces stop-and-wait)"
        );
    }

    #[test]
    fn windowed_initial_window_is_32_mib_when_granted() {
        let fc = FlowControl::new_windowed(INITIAL_DELAYED_ACK_BYTES);
        assert!(fc.is_windowed(), "windowed mode must report windowed");
        assert_eq!(
            fc.available_bytes(),
            Some(32 * 1024 * 1024),
            "granted window must be exactly 32 MiB"
        );
        assert!(fc.can_send(), "a freshly granted window permits sending");
    }

    #[test]
    fn opener_starts_at_zero_and_blocks_until_credited() {
        // Opener rule: own send window starts at 0 until the device's first OKAY.
        let mut fc = FlowControl::new_windowed(0);
        assert!(
            !fc.can_send(),
            "opener with a zero window must not send before being credited"
        );
        // Device grants 32 MiB via the first OKAY payload.
        let grant = INITIAL_DELAYED_ACK_BYTES;
        let payload = (i32::try_from(grant).expect("fits i32")).to_le_bytes();
        assert!(fc.on_okay_payload(&payload), "valid 4-byte OKAY accepted");
        assert_eq!(
            fc.available_bytes(),
            Some(grant),
            "first OKAY must credit the opener's window from 0 to the grant"
        );
        assert!(fc.can_send(), "credited opener may now send");
    }

    #[test]
    fn record_sent_debits_window() {
        let mut fc = FlowControl::new_windowed(1000);
        fc.record_sent(400);
        assert_eq!(
            fc.available_bytes(),
            Some(600),
            "sending 400 bytes must debit the window by 400"
        );
    }

    #[test]
    fn window_exhaustion_blocks_then_recovers_after_okay() {
        let mut fc = FlowControl::new_windowed(500);
        // Final chunk may drive the window <= 0 (AOSP allows one over-send).
        fc.record_sent(500);
        assert_eq!(fc.available_bytes(), Some(0), "window drained to exactly 0");
        assert!(!fc.can_send(), "a zero window blocks the next send");
        // OKAY credits 300 bytes back.
        assert!(fc.on_okay_payload(&300_i32.to_le_bytes()));
        assert_eq!(
            fc.available_bytes(),
            Some(300),
            "OKAY delta must credit the window back"
        );
        assert!(fc.can_send(), "recovered window permits sending again");
    }

    #[test]
    fn window_may_go_negative_via_oversend() {
        let mut fc = FlowControl::new_windowed(100);
        fc.record_sent(250); // one over-send (AOSP permits a single in-flight chunk)
        assert_eq!(
            fc.available_bytes(),
            Some(-150),
            "the window is signed and may go negative after an over-send"
        );
        assert!(!fc.can_send(), "a negative window blocks the next send");
    }

    #[test]
    fn negative_delta_is_applied_without_panic() {
        let mut fc = FlowControl::new_windowed(1000);
        // Preemptive backpressure: a negative delta shrinks the window.
        assert!(
            fc.on_okay_payload(&(-400_i32).to_le_bytes()),
            "a negative i32 delta is a valid (signed) OKAY"
        );
        assert_eq!(
            fc.available_bytes(),
            Some(600),
            "negative delta must subtract from the window without underflow panic"
        );
    }

    #[test]
    fn classic_empty_payload_okay_is_noop() {
        let mut fc = FlowControl::new_classic();
        assert!(
            fc.on_okay_payload(&[]),
            "empty OKAY payload is the classic rendezvous, accepted"
        );
        assert_eq!(
            fc.available_bytes(),
            None,
            "classic mode stays windowless after an empty OKAY"
        );
    }

    #[test]
    fn empty_payload_in_windowed_mode_is_noop_credit() {
        let mut fc = FlowControl::new_windowed(1000);
        assert!(
            fc.on_okay_payload(&[]),
            "an empty payload is a 0-delta no-op"
        );
        assert_eq!(
            fc.available_bytes(),
            Some(1000),
            "empty OKAY must not change the window"
        );
    }

    #[test]
    fn malformed_okay_payload_is_rejected() {
        let mut fc = FlowControl::new_windowed(1000);
        assert!(
            !fc.on_okay_payload(&[1, 2, 3]),
            "a 3-byte payload is neither classic (0) nor a valid delta (4) → rejected"
        );
        assert!(
            !fc.on_okay_payload(&[1, 2, 3, 4, 5, 6, 7, 8]),
            "an 8-byte payload is rejected (only 0 or 4 are valid)"
        );
        assert_eq!(
            fc.available_bytes(),
            Some(1000),
            "a rejected payload must leave the window unchanged"
        );
    }

    #[test]
    fn i32_le_round_trip_through_okay_payload() {
        for value in [
            0_i32,
            1,
            -1,
            65536,
            -65536,
            i32::MAX,
            i32::MIN,
            32 * 1024 * 1024,
        ] {
            let mut fc = FlowControl::new_windowed(0);
            let payload = value.to_le_bytes();
            assert!(
                fc.on_okay_payload(&payload),
                "valid 4-byte payload for {value}"
            );
            assert_eq!(
                fc.available_bytes(),
                Some(i64::from(value)),
                "i32 LE {value} must round-trip into the window delta"
            );
        }
    }

    #[test]
    fn encode_okay_payload_windowed_is_i32_le() {
        let payload = encode_okay_payload(true, 65536);
        assert_eq!(
            payload,
            65536_i32.to_le_bytes().to_vec(),
            "windowed OKAY payload must be the byte count as 4-byte LE i32"
        );
    }

    #[test]
    fn encode_okay_payload_classic_is_empty() {
        assert!(
            encode_okay_payload(false, 65536).is_empty(),
            "classic OKAY payload must be empty regardless of bytes delivered"
        );
    }

    #[test]
    fn encode_okay_payload_clamps_huge_byte_count() {
        // Defensive: a single delivered chunk never exceeds MAX_PAYLOAD, but a
        // value > i32::MAX must clamp rather than wrap/panic.
        let huge = usize::try_from(i64::from(i32::MAX) + 1000).expect("fits usize");
        let payload = encode_okay_payload(true, huge);
        assert_eq!(
            payload,
            i32::MAX.to_le_bytes().to_vec(),
            "byte counts above i32::MAX clamp to i32::MAX"
        );
    }

    #[test]
    fn overflow_accumulation_does_not_panic() {
        let mut fc = FlowControl::new_windowed(i64::MAX - 10);
        // Repeated max positive deltas must saturate, never panic.
        for _ in 0..5 {
            assert!(fc.on_okay_payload(&i32::MAX.to_le_bytes()));
        }
        assert_eq!(
            fc.available_bytes(),
            Some(i64::MAX),
            "accumulating past i64::MAX must saturate rather than overflow-panic"
        );
    }

    #[test]
    fn max_payload_constants() {
        assert_eq!(MAX_PAYLOAD, 1024 * 1024, "MAX_PAYLOAD = 1 MiB");
        assert_eq!(
            INITIAL_DELAYED_ACK_BYTES,
            32 * 1024 * 1024,
            "INITIAL_DELAYED_ACK_BYTES = 32 MiB"
        );
    }
}
