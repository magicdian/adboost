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
use super::scenario::{OpenResponse, Scenario};
use crate::message_devices::usb::flow_control::{INITIAL_DELAYED_ACK_BYTES, encode_okay_payload};

/// A fixed 20-byte AUTH challenge token. adbd's real token is random; the host
/// signs whatever bytes it receives, so a constant is faithful for the handshake
/// (the simulated device accepts by policy, not by verifying the signature —
/// see [`SimState::react_to`]).
const AUTH_CHALLENGE_TOKEN: [u8; 20] = [0x5a; 20];

/// The device's own local-id for the session it accepts. The host routes inbound
/// frames by their `arg1` (its own local id); this is the device's `arg0` on
/// replies (the host stores it as `remote_id`). Any fixed non-zero value works.
const DEVICE_LOCAL_ID: u32 = 0x5151_5151;

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
    /// The single active session's host-side local id (the OPEN's `arg0`), set
    /// when the device accepts a host `OPEN`. `None` before any session is open.
    session_host_id: Option<u32>,
    /// Whether the active session has already seen its first host `WRTE` (drives
    /// `close_after_first_write`, which must fire exactly once).
    session_first_write_seen: bool,
    /// Count of `write_message` calls seen so far (drives `write_fault`).
    writes_seen: u32,
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
            session_host_id: None,
            session_first_write_seen: false,
            writes_seen: 0,
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

    /// Force the device's reader to die on its next read (and stay dead) — the
    /// adbd-restart edge. Latches `reader_dead` so `should_die_on_idle_read` is
    /// not even needed; the next `read_message` returns the fatal error.
    pub(super) fn kill_reader(&mut self) {
        self.reader_dead = true;
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

    /// Account for one `write_message` call and report whether this write should
    /// be the injected `write_fault`. Returns `Some(after_bytes)` when it should
    /// fail (`after_bytes > 0` = fatal mid-frame truncation, `0` = recoverable
    /// backpressure with nothing committed), else `None`. Used by
    /// [`super::chunked::ChunkedTransport`] (byte-level B7/B9).
    pub(super) fn take_write_fault(&mut self) -> Option<usize> {
        self.writes_seen += 1;
        match self.scenario.write_fault {
            Some(f) if f.on_write == self.writes_seen => Some(f.after_bytes),
            _ => None,
        }
    }

    /// How many device frames to coalesce into one read delivery (B5
    /// over-delivery). `1` = no coalescing.
    pub(super) fn coalesce_frames(&self) -> usize {
        self.scenario.coalesce_frames.unwrap_or(1).max(1)
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
            // Session-level reactions once the handshake is complete.
            (Phase::Connected, MessageCommand::Open) => self.react_to_open(msg),
            (Phase::Connected, MessageCommand::Write) => self.react_to_write(msg),
            (Phase::Connected, MessageCommand::Clse) => self.react_to_clse(msg),
            // OKAY from the host is a flow-control credit / readiness poke; the
            // device does not need to reply to it. Repeated CNXN writes (e.g.
            // do_connect retrying after a transient read error) once already
            // connected are likewise a no-op — the banner reply is already queued.
            _ => {}
        }
    }

    /// React to a host `OPEN`, per the scenario's [`OpenResponse`] policy. The
    /// host's `arg0` is its local id; the device routes replies back with
    /// `arg1 = host_local_id` (the host's reader keys on `arg1`).
    fn react_to_open(&mut self, msg: &ADBTransportMessage) {
        let host_id = msg.header().arg0();
        let windowed = self.windowed_session();
        match self.scenario.open_response {
            OpenResponse::Accept => {
                self.session_host_id = Some(host_id);
                self.session_first_write_seen = false;
                self.enqueue_okay(host_id, windowed);
                self.enqueue_post_open_writes(host_id);
            }
            OpenResponse::AcceptDoubleOkay => {
                self.session_host_id = Some(host_id);
                self.session_first_write_seen = false;
                // Two OKAYs back-to-back: the host's open must tolerate the extra.
                self.enqueue_okay(host_id, windowed);
                self.enqueue_okay(host_id, windowed);
            }
            OpenResponse::RejectWithClse => {
                // CLSE(arg0 = 0, arg1 = host_local_id) on the data channel — the
                // AOSP rejection. The host must fast-fail, not hang (bug #3a).
                self.enqueue(MessageCommand::Clse, 0, host_id, &[]);
            }
            OpenResponse::Ignore => {
                // Send nothing: the host hits its OPEN-response timeout.
            }
        }
    }

    /// Enqueue the scenario's post-open device→host `WRTE` chunks (one frame per
    /// chunk), then an optional trailing `CLSE`. Lets the device "produce" a
    /// stream of bytes (e.g. encoded shell-v2 frames) without a host write.
    fn enqueue_post_open_writes(&mut self, host_id: u32) {
        if self.scenario.post_open_writes.is_empty() {
            return;
        }
        for chunk in self.scenario.post_open_writes.clone() {
            self.enqueue(MessageCommand::Write, DEVICE_LOCAL_ID, host_id, &chunk);
        }
        if self.scenario.close_after_post_open {
            self.enqueue(MessageCommand::Clse, DEVICE_LOCAL_ID, host_id, &[]);
        }
    }

    /// React to a host `WRTE`: acknowledge with `OKAY`, optionally echo bytes
    /// back as a device `WRTE`, and optionally close the stream early.
    fn react_to_write(&mut self, msg: &ADBTransportMessage) {
        let Some(host_id) = self.session_host_id else {
            return; // WRTE with no open session — drop (a real adbd would too).
        };
        let windowed = self.windowed_session();
        // Acknowledge the host's WRTE with a crediting OKAY.
        self.enqueue_okay(host_id, windowed);

        let first_write = !self.session_first_write_seen;
        // A crafted first-write reply (e.g. a truncated SYNC frame) takes priority
        // over the generic echo and fires only on the first WRTE.
        if first_write && let Some(bytes) = self.scenario.first_write_reply.clone() {
            self.enqueue(MessageCommand::Write, DEVICE_LOCAL_ID, host_id, &bytes);
        } else if let Some(n) = self.scenario.echo_bytes {
            let take = n.min(msg.payload().len());
            let echo: Vec<u8> = msg.payload()[..take].to_vec();
            self.enqueue(MessageCommand::Write, DEVICE_LOCAL_ID, host_id, &echo);
        }

        if self.scenario.close_after_first_write && first_write {
            self.enqueue(MessageCommand::Clse, DEVICE_LOCAL_ID, host_id, &[]);
        }
        self.session_first_write_seen = true;
    }

    /// React to a host `CLSE`: mirror a `CLSE` back and forget the session.
    fn react_to_clse(&mut self, msg: &ADBTransportMessage) {
        if let Some(host_id) = self.session_host_id.take() {
            self.enqueue(MessageCommand::Clse, DEVICE_LOCAL_ID, host_id, &[]);
            let _ = msg; // arg matching not needed; one active session in this model
        }
        self.session_first_write_seen = false;
    }

    /// Whether this connection negotiated windowed flow control — mirrors the
    /// host's `delayed_ack` gate so the device's OKAY payloads match the mode the
    /// host negotiated (else the host rejects a 4-byte OKAY in classic mode).
    fn windowed_session(&self) -> bool {
        crate::models::DeviceFeatureSet::from_banner(&self.profile.banner).delayed_ack
            && self.profile.version >= super::profile::A_VERSION_SKIP_CHECKSUM
    }

    /// Enqueue an `OKAY(arg0 = device_local_id, arg1 = host_local_id)` carrying a
    /// window grant (windowed) or empty payload (classic), built with the
    /// production `encode_okay_payload` so the on-wire bytes match exactly.
    fn enqueue_okay(&mut self, host_id: u32, windowed: bool) {
        let grant = usize::try_from(INITIAL_DELAYED_ACK_BYTES).unwrap_or(usize::MAX);
        let payload = encode_okay_payload(windowed, grant);
        self.enqueue(MessageCommand::Okay, DEVICE_LOCAL_ID, host_id, &payload);
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
