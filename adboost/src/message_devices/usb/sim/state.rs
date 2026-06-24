//! The shared adbd state machine behind a [`SimulatedDevice`].
//!
//! [`SimState`] is the single source of truth shared (behind an `Arc<Mutex<_>>`)
//! by the transport's reader and writer clones. It owns the device handshake
//! phase, the outbound frame queue, and the fault counters drawn from the
//! [`Scenario`]. [`SimState::react_to`] is the pure request→response step the
//! transport calls on every host write; the transport layer ([`super::device`])
//! owns all the `async` / locking / timeout plumbing so this stays I/O-free and
//! directly unit-testable.

use std::collections::VecDeque;

use crate::message_devices::adb_transport_message::{
    ADBTransportMessage, AUTH_RSAPUBLICKEY, AUTH_SIGNATURE, AUTH_TOKEN,
};
use crate::message_devices::message_commands::MessageCommand;

use super::profile::DeviceProfile;
use super::scenario::Scenario;

/// A fixed 20-byte AUTH challenge token. adbd's real token is random; the host
/// signs whatever bytes it receives, so a constant is faithful for the handshake
/// (the simulated device accepts by policy, not by verifying the signature —
/// see [`SimState::react_to`]).
const AUTH_CHALLENGE_TOKEN: [u8; 20] = [0x5a; 20];

/// Where the device is in its CNXN/AUTH handshake. Drives [`SimState::react_to`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum Phase {
    /// Waiting for the host's CNXN. The opening state.
    AwaitingCnxn,
    /// Sent `AUTH(TOKEN)`; waiting for the host's `AUTH(SIGNATURE)`.
    AwaitingSignature,
    /// Rejected the first signature; waiting for the host's `AUTH(RSAPUBLICKEY)`.
    AwaitingPublicKey,
    /// Handshake complete — the device has sent its CNXN banner. Session-level
    /// reactions (OPEN/OKAY/WRTE/CLSE) land here in Phase B.
    Connected,
}

/// The mutable state of a simulated adbd, shared across the transport's clones.
pub(super) struct SimState {
    /// The device version/banner/auth axis.
    profile: DeviceProfile,
    /// The fault/lifecycle injection axis.
    scenario: Scenario,
    /// Current handshake phase.
    phase: Phase,
    /// Frames the device has decided to send, awaiting the host's reads (FIFO).
    outbound: VecDeque<ADBTransportMessage>,
    /// Stale `CLSE` replies still to emit (ahead of the real CNXN) on connect.
    stale_clse_remaining: u32,
    /// Transient `write_message` failures still to emit before writes succeed.
    transient_writes_remaining: u32,
    /// Transient `read_message` failures still to emit (only consumed when a
    /// frame is actually queued — a transient models the controller failing to
    /// deliver a frame that *is* waiting, not an idle pipe).
    transient_reads_remaining: u32,
    /// Reads served since reaching [`Phase::Connected`] — drives `die_after_reads`.
    reads_while_connected: u32,
    /// Set once the reader half has emitted its fatal death error, so it stays
    /// dead on every subsequent read (a re-enumeration handle never revives).
    reader_dead: bool,
}

impl SimState {
    /// Build the initial state for a profile + scenario.
    pub(super) fn new(profile: DeviceProfile, scenario: Scenario) -> Self {
        let stale_clse_remaining = scenario.stale_clse;
        let transient_writes_remaining = scenario.transient_writes;
        let transient_reads_remaining = scenario.transient_reads;
        Self {
            profile,
            scenario,
            phase: Phase::AwaitingCnxn,
            outbound: VecDeque::new(),
            stale_clse_remaining,
            transient_writes_remaining,
            transient_reads_remaining,
            reads_while_connected: 0,
            reader_dead: false,
        }
    }

    /// The transfer error this device's scenario emits for transient faults.
    pub(super) fn transient_err(&self) -> nusb::transfer::TransferError {
        self.scenario.transient_err()
    }

    /// Whether the outbound queue currently has a frame for the host to read.
    pub(super) fn has_outbound(&self) -> bool {
        !self.outbound.is_empty()
    }

    /// Pop the next frame the device wants to send, if any.
    pub(super) fn pop_outbound(&mut self) -> Option<ADBTransportMessage> {
        self.outbound.pop_front()
    }

    /// Whether the device has finished its handshake (sent its CNXN banner).
    pub(super) fn is_connected(&self) -> bool {
        self.phase == Phase::Connected
    }

    // -- write-side fault accounting ---------------------------------------

    /// Consume one transient-write credit if any remain; returns `true` when the
    /// caller should emit a transient write error instead of delivering the write.
    pub(super) fn take_transient_write(&mut self) -> bool {
        if self.transient_writes_remaining > 0 {
            self.transient_writes_remaining -= 1;
            true
        } else {
            false
        }
    }

    // -- read-side fault accounting ----------------------------------------

    /// Consume one transient-read credit if any remain AND a frame is queued
    /// (a transient is a delivery failure of a pending frame, not an idle pipe);
    /// returns `true` when the caller should emit a transient read error.
    pub(super) fn take_transient_read(&mut self) -> bool {
        if self.transient_reads_remaining > 0 && self.has_outbound() {
            self.transient_reads_remaining -= 1;
            true
        } else {
            false
        }
    }

    /// Whether the reader half has already latched dead (a re-enumeration handle
    /// never revives). Checked first on every read.
    pub(super) fn reader_already_dead(&self) -> bool {
        self.reader_dead
    }

    /// Whether this **idle** read (no frame queued) should be the reader's fatal
    /// death, per `die_after_reads`.
    ///
    /// Counts only idle reads served while [`Phase::Connected`], so it never eats
    /// a real reply frame (the caller delivers any queued frame first) and the
    /// death lands in the live reader loop *after* the handshake — modeling adbd
    /// dropping an established, quiet connection (an `adb root`/`unroot` restart).
    /// Once it fires, the reader stays latched dead.
    pub(super) fn should_die_on_idle_read(&mut self) -> bool {
        if let Some(n) = self.scenario.die_after_reads
            && self.phase == Phase::Connected
        {
            self.reads_while_connected += 1;
            if self.reads_while_connected >= n {
                self.reader_dead = true;
                return true;
            }
        }
        false
    }

    /// React to one frame the host wrote, enqueuing the device's reply frames.
    ///
    /// This is the request→response core. It is intentionally total and
    /// side-effect-free beyond mutating `self`: any frame that does not advance
    /// the current phase is ignored (a real adbd would drop unexpected control
    /// frames too). Session-level commands (OPEN/OKAY/WRTE/CLSE) are no-ops in
    /// Phase A and are handled once [`Phase::Connected`] gains a session model in
    /// Phase B.
    pub(super) fn react_to(&mut self, msg: &ADBTransportMessage) {
        let command = msg.header().command();
        match (self.phase, command) {
            (Phase::AwaitingCnxn, MessageCommand::Cnxn) => {
                if self.stale_clse_remaining > 0 {
                    // Emit a stale CLSE ahead of the real reply, modeling a prior
                    // connection's orphaned stream. The host drains + retries CNXN.
                    self.stale_clse_remaining -= 1;
                    self.enqueue(MessageCommand::Clse, 0, 0, &[]);
                } else if self.profile.requires_auth {
                    self.enqueue(MessageCommand::Auth, AUTH_TOKEN, 0, &AUTH_CHALLENGE_TOKEN);
                    self.phase = Phase::AwaitingSignature;
                } else {
                    self.send_cnxn_banner();
                    self.phase = Phase::Connected;
                }
            }
            (Phase::AwaitingSignature, MessageCommand::Auth)
                if msg.header().arg0() == AUTH_SIGNATURE =>
            {
                // The device accepts by policy (it does not hold the host's
                // public key to verify against — faithful enough for the
                // handshake state machine; real signature verification is an
                // adbd concern out of this harness's scope).
                if self.profile.accepts_first_signature {
                    self.send_cnxn_banner();
                    self.phase = Phase::Connected;
                } else {
                    // Reject once: re-challenge, forcing the host's pubkey path.
                    self.enqueue(MessageCommand::Auth, AUTH_TOKEN, 0, &AUTH_CHALLENGE_TOKEN);
                    self.phase = Phase::AwaitingPublicKey;
                }
            }
            (Phase::AwaitingPublicKey, MessageCommand::Auth)
                if msg.header().arg0() == AUTH_RSAPUBLICKEY =>
            {
                self.send_cnxn_banner();
                self.phase = Phase::Connected;
            }
            // Repeated CNXN writes (e.g. do_connect retrying after a transient
            // read error) once already connected are a no-op: the banner reply is
            // already queued, so we must not double-enqueue it.
            _ => {}
        }
    }

    /// Enqueue the device's CNXN banner reply, carrying its profile version/banner.
    fn send_cnxn_banner(&mut self) {
        let version = self.profile.version;
        let banner = self.profile.banner.clone();
        self.enqueue(MessageCommand::Cnxn, version, 1_048_576, banner.as_bytes());
    }

    /// Build a frame and push it onto the outbound queue. Construction uses the
    /// same `ADBTransportMessage::try_new` the production code uses; a build
    /// failure (only possible for an oversize payload, never for these control
    /// frames) is dropped rather than panicking.
    fn enqueue(&mut self, command: MessageCommand, arg0: u32, arg1: u32, payload: &[u8]) {
        if let Ok(msg) = ADBTransportMessage::try_new(command, arg0, arg1, payload) {
            self.outbound.push_back(msg);
        }
    }
}
