//! Phase A handshake suite for the [`SimulatedDevice`] / [`ChunkedTransport`]
//! harness: CNXN, stale-CLSE drain, AUTH, and `delayed_ack` negotiation driven
//! end-to-end through the real [`PersistentConnection`], plus named regression
//! locks for the bugs that escaped to downstream consumers.
//!
//! These run under `cargo test --features usb` (the persistent connection lives
//! behind `usb`); they need no hardware and no network.

use std::path::PathBuf;
use std::time::Duration;

use nusb::transfer::TransferError;

use super::{ChunkedTransport, DeviceProfile, Scenario, SimulatedDevice};
use crate::RustADBError;
use crate::message_devices::adb_message_transport::ADBMessageTransport;
use crate::message_devices::adb_transport_message::ADBTransportMessage;
use crate::message_devices::message_commands::MessageCommand;
use crate::message_devices::usb::persistent::PersistentConnection;
use crate::models::DeviceFeatureSet;

/// A key path that does not exist, so `new_with_features` falls back to a freshly
/// generated random key (see `read_adb_private_key`: `NotFound` → `Ok(None)`).
/// Keeps every test hermetic — no dependence on `~/.android/adbkey`.
fn ephemeral_key_path() -> PathBuf {
    PathBuf::from("/nonexistent/adboost-sim-tests/this-key-does-not-exist")
}

/// Build a persistent connection over a frame-level simulated device advertising
/// our default feature set (windowing on). Drives the real CNXN/AUTH handshake.
async fn connect_default(
    device: SimulatedDevice,
) -> crate::Result<PersistentConnection<SimulatedDevice>> {
    PersistentConnection::new_with_features(
        device,
        Some(ephemeral_key_path()),
        DeviceFeatureSet::default(),
    )
    .await
}

/// Build a connection advertising an explicit feature set.
async fn connect_with(
    device: SimulatedDevice,
    features: DeviceFeatureSet,
) -> crate::Result<PersistentConnection<SimulatedDevice>> {
    PersistentConnection::new_with_features(device, Some(ephemeral_key_path()), features).await
}

// ===========================================================================
// CNXN handshake (CNXN-1..6, CNXN-11)
// ===========================================================================

/// CNXN-1/CNXN-2: a healthy device answers CNXN; the handshake completes and the
/// reported peer banner reflects the device's profile.
#[tokio::test(start_paused = true)]
async fn cnxn_completes_and_parses_peer_banner() {
    let conn = connect_default(SimulatedDevice::new(DeviceProfile::android_16()))
        .await
        .expect("handshake must complete against a healthy Android-16 device");
    assert!(
        conn.peer_features().shell_v2,
        "peer banner advertised shell_v2 → peer_features must reflect it"
    );
    assert!(
        conn.peer_features().delayed_ack,
        "Android-16 banner advertises delayed_ack → peer_features must reflect it"
    );
    assert!(conn.is_alive(), "a freshly handshaked connection is alive");
}

/// CNXN-5: a feature-less device (empty `features=`) parses to the all-false peer
/// set — the conservative result the server relies on (B-feat groundwork).
#[tokio::test(start_paused = true)]
async fn cnxn_featureless_banner_yields_all_false_peer_features() {
    let conn = connect_default(SimulatedDevice::new(DeviceProfile::featureless()))
        .await
        .expect("handshake completes against a feature-less device");
    assert_eq!(
        conn.peer_features(),
        &DeviceFeatureSet::from_banner("device::features="),
        "an empty features= segment must parse to the all-false peer set"
    );
    assert!(
        !conn.peer_features().shell_v2 && !conn.peer_features().delayed_ack,
        "a feature-less device must advertise neither shell_v2 nor delayed_ack"
    );
}

// ===========================================================================
// Transient connect errors (CNXN-7..10) — generalizes the ScriptedTransport
// retry tests onto the full PersistentConnection::new path.
// ===========================================================================

/// CNXN-7: a few transient `NotResponding` write blips within the in-place budget
/// are ridden out and the handshake completes. Generalizes
/// `do_connect_retries_transient_notresponding_then_succeeds` to full `new`.
#[tokio::test(start_paused = true)]
async fn cnxn_retries_then_succeeds_on_transient_notresponding() {
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        Scenario::healthy().with_transient_writes(2, TransferError::Unknown(0xe000_02ed)),
    );
    let conn = connect_default(device)
        .await
        .expect("must ride out NotResponding transients within the in-place budget");
    assert!(
        conn.is_alive(),
        "connection is alive after riding out blips"
    );
}

/// CNXN-8: same, with `Disconnected` (`NoDevice`) transients.
#[tokio::test(start_paused = true)]
async fn cnxn_retries_then_succeeds_on_transient_disconnected() {
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        Scenario::healthy().with_transient_writes(2, TransferError::Disconnected),
    );
    connect_default(device)
        .await
        .expect("must ride out Disconnected transients within the in-place budget");
}

/// CNXN-9/CNXN-10: transient blips exceeding the small in-place budget make the
/// handshake fail fast (propagate the error) rather than burn the full CNXN
/// budget — the back-to-back root/unroot anti-amplification invariant. The error
/// then propagates to the outer reopen layer (modeled in Phase C).
#[tokio::test(start_paused = true)]
async fn cnxn_fails_fast_when_transients_exceed_in_place_budget() {
    // u32::MAX transient writes = a permanently-dead handle.
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        Scenario::healthy().with_transient_writes(u32::MAX, TransferError::Disconnected),
    );
    let err = connect_default(device)
        .await
        .err()
        .expect("a permanently-dead handle must fail the handshake, not succeed");
    assert!(
        matches!(
            err,
            RustADBError::UsbTransferError(TransferError::Disconnected)
        ),
        "a permanently-dead handle must propagate Disconnected fast (handing off \
         to the outer reopen layer), not exhaust the CNXN budget; got {err:?}"
    );
}

/// CNXN-11: a transient error on the *read* side (write succeeded, the reply
/// delivery blips) is ridden out by the same in-place retry path.
#[tokio::test(start_paused = true)]
async fn cnxn_retries_then_succeeds_on_transient_read() {
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        Scenario::healthy().with_transient_reads(2, TransferError::Unknown(0xe000_02ed)),
    );
    connect_default(device)
        .await
        .expect("a transient on the CNXN read must be retried in place like a write");
}

// ===========================================================================
// Stale-CLSE drain (DRAIN-1..2)
// ===========================================================================

/// DRAIN-1: a single stale CLSE ahead of the real CNXN is drained and the
/// handshake retries to success.
#[tokio::test(start_paused = true)]
async fn cnxn_drains_single_stale_clse_then_succeeds() {
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        Scenario::healthy().with_stale_clse(1),
    );
    connect_default(device)
        .await
        .expect("a single stale CLSE must be drained, then CNXN retried to success");
}

/// DRAIN-2: a burst of several stale CLSEs (more than the old fixed-3 bound, well
/// under `CNXN_MAX_ATTEMPTS`) is still drained to a successful handshake.
#[tokio::test(start_paused = true)]
async fn cnxn_drains_burst_of_stale_clse_then_succeeds() {
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        Scenario::healthy().with_stale_clse(5),
    );
    connect_default(device)
        .await
        .expect("a burst of stale CLSEs (<CNXN_MAX_ATTEMPTS) must drain to success");
}

// ===========================================================================
// AUTH flow (AUTH-1, AUTH-2)
// ===========================================================================

/// AUTH-1: TOKEN → SIGNATURE → CNXN. A device that demands AUTH and accepts the
/// signature completes the handshake.
#[tokio::test(start_paused = true)]
async fn auth_token_signature_then_connected() {
    connect_default(SimulatedDevice::new(DeviceProfile::auth_known_key()))
        .await
        .expect("TOKEN→SIGNATURE→CNXN auth path must complete");
}

/// AUTH-2: TOKEN → SIGNATURE → (reject) → RSAPUBLICKEY → CNXN. A device that
/// rejects the first signature forces the public-key path, which then completes.
#[tokio::test(start_paused = true)]
async fn auth_falls_through_to_public_key_then_connected() {
    connect_default(SimulatedDevice::new(DeviceProfile::auth_new_key()))
        .await
        .expect("TOKEN→SIGNATURE→re-challenge→RSAPUBLICKEY→CNXN path must complete");
}

// ===========================================================================
// delayed_ack negotiation (DACK-1..4) — observed end-to-end via the gated
// `delayed_ack_negotiated()` accessor.
// ===========================================================================

/// DACK-1: both ends advertise + version capable → windowed negotiated.
#[tokio::test(start_paused = true)]
async fn dack_windowed_when_both_advertise_and_version_capable() {
    let conn = connect_default(SimulatedDevice::new(DeviceProfile::android_16()))
        .await
        .expect("handshake completes");
    assert!(
        conn.delayed_ack_negotiated(),
        "Android-16 (banner has delayed_ack, version >= SKIP_CHECKSUM) must negotiate windowed"
    );
}

/// DACK-2/B1 REGRESSION: a device whose banner lacks `delayed_ack` (Android-11
/// era) must negotiate **classic** stop-and-wait — even though we advertise the
/// feature. This is the version/feature gate whose absence (windowed OPEN at a
/// non-windowing peer) caused `open_session` to hang 10s on real hardware (bug
/// #1). Today this needs a real old device; the sim makes it deterministic.
#[tokio::test(start_paused = true)]
async fn android_11_negotiates_classic_flow_control() {
    let conn = connect_default(SimulatedDevice::new(DeviceProfile::android_11()))
        .await
        .expect("handshake completes against an Android-11 device");
    assert!(
        !conn.delayed_ack_negotiated(),
        "B1: an Android-11 device (no delayed_ack in banner, legacy version) MUST \
         negotiate classic flow control, never windowed — advertising windowed to \
         a non-windowing peer makes adbd ignore the windowed OPEN (10s hang)"
    );
}

/// DACK-4: even against a fully-capable device, a local opt-out
/// (`delayed_ack=false`) must negotiate classic — we never window when we did not
/// advertise it.
#[tokio::test(start_paused = true)]
async fn dack_local_opt_out_negotiates_classic() {
    let features = DeviceFeatureSet {
        delayed_ack: false,
        ..DeviceFeatureSet::default()
    };
    let conn = connect_with(SimulatedDevice::new(DeviceProfile::android_16()), features)
        .await
        .expect("handshake completes");
    assert!(
        !conn.delayed_ack_negotiated(),
        "a local delayed_ack opt-out must negotiate classic even against a capable device"
    );
}

// ===========================================================================
// B2 REGRESSION: data_check=0 frames are accepted (magic-only integrity).
// ===========================================================================

/// B2 REGRESSION: a payload-bearing CNXN banner built by the production
/// `try_new` carries a non-zero byte-sum `data_check`, but the receive path must
/// accept frames regardless of `data_check` (magic-only integrity). The
/// Android-16 handshake exercises exactly this: its banner is payload-bearing and
/// is accepted. We additionally assert the integrity contract directly on a
/// `data_check=0` frame to lock bug #2 (every payload frame from a skip-checksum
/// peer was rejected when crc was still compared).
#[tokio::test(start_paused = true)]
async fn data_check_is_not_validated_on_receive() {
    use crate::message_devices::adb_transport_message::ADBTransportMessageHeader;

    // The full handshake already proves a payload-bearing banner is accepted.
    connect_default(SimulatedDevice::new(DeviceProfile::android_16()))
        .await
        .expect("a payload-bearing CNXN banner must be accepted (magic-only integrity)");

    // Directly: a frame with a deliberately wrong data_check but correct magic
    // must still pass the integrity check (the bug #2 lock, mirrored here for the
    // sim path so the regression is named in this suite too).
    let good = ADBTransportMessage::try_new(MessageCommand::Write, 1, 2, b"skip-checksum-peer")
        .expect("build frame");
    let forged = ADBTransportMessage::from_header_and_payload(
        ADBTransportMessageHeader::try_new(MessageCommand::Write, 1, 2, &[])
            .expect("header with data_check for empty payload (i.e. 0)"),
        good.into_payload(),
    );
    assert!(
        forged.check_message_integrity(),
        "B2: a payload-bearing frame whose data_check does not match its payload \
         (here 0, as a skip-checksum peer sends) MUST still pass magic-only integrity"
    );
}

// ===========================================================================
// Liveness / death seam (LIV-13 groundwork): reader death fires the signal.
// ===========================================================================

/// LIV-13: once the handshake is complete, a device whose reader dies (adbd
/// closed the connection) must make `is_alive()` flip false and `wait_closed()`
/// resolve. This is the death seam the Phase-C `TransportReset`/wait-for-disconnect
/// path builds on; here we prove the message-transport layer surfaces the edge.
#[tokio::test]
async fn reader_death_after_handshake_flips_is_alive_and_resolves_wait_closed() {
    // Real-time test (not paused): the reader loop runs on its own task and we
    // wait for the death edge to propagate.
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        // Die on the first idle read after the handshake completes.
        Scenario::healthy().with_death_after_reads(1),
    );
    let conn = connect_default(device)
        .await
        .expect("handshake completes before the reader dies");

    // The reader's next idle read returns the fatal Disconnected → DeathSignal.
    tokio::time::timeout(Duration::from_secs(5), conn.wait_closed())
        .await
        .expect("wait_closed must resolve once the reader hits its fatal death");
    assert!(
        !conn.is_alive(),
        "a connection whose reader died must report not-alive"
    );
}

/// Regression lock for the downstream-reported "reader single-sided death pins
/// the USB `Interface` claim forever" bug.
///
/// When ONLY the reader dies (fatal read) the writer used to park on
/// `writer_rx.recv()` forever — the connection struct keeps a live
/// `WriterHandle` sender, so the channel never closes on its own — pinning the
/// writer's transport clone, and thus the shared OS claim, until the LAST
/// external `Arc<PersistentConnection>` dropped (which a long-lived relay/proxy
/// holder never triggers). A `SimulatedDevice` clone is the structural analogue
/// of a `USBTransport` clone: both share the underlying handle via an `Arc`, and
/// the resource frees only when the last clone drops. So the writer-clone leak
/// shows up as `state_strong_count()` never falling back to the external-only
/// count while the connection is still held alive.
///
/// On PRE-fix code the writer never wakes, the count stays at the
/// reader-released level (writer clone pinned), and this test times out. POST-fix
/// the writer races `recv()` against the death signal in a `select!`, wakes on
/// the death edge, drops its clone, and the count falls to the external-only
/// count — WITHOUT dropping the `Arc<PersistentConnection>` and WITHOUT closing
/// the writer channel.
#[tokio::test]
async fn reader_death_releases_writer_transport_clone_without_dropping_connection() {
    // Real-time test (not paused): we wait for the death edge to propagate from
    // the reader task to the writer task across the shared DeathSignal.
    let device = SimulatedDevice::with_scenario(
        DeviceProfile::android_16(),
        // Die on the first idle read after the handshake completes.
        Scenario::healthy().with_death_after_reads(1),
    );
    // External probe clone, held for the whole test — this is the "long-lived
    // holder" whose continued existence used to pin the claim.
    let probe = device.clone();

    let conn = connect_default(device)
        .await
        .expect("handshake completes before the reader dies");

    // Sanity precondition: after construction both I/O tasks each hold a clone, so
    // alongside the external probe the shared state has strictly more than one
    // live clone (reader + writer + probe). The exact number is asserted as the
    // observed invariant; the load-bearing claim is that it must DROP after death.
    let initial = probe.state_strong_count();
    assert!(
        initial > 1,
        "after construction the reader and writer tasks each hold a transport clone \
         (observed strong_count = {initial}); the external probe is additional"
    );

    // Wait for the reader's fatal death to fire the DeathSignal.
    tokio::time::timeout(Duration::from_secs(5), conn.wait_closed())
        .await
        .expect("wait_closed must resolve once the reader hits its fatal death");

    // The writer's death-edge wakeup + clone drop is asynchronous after
    // wait_closed resolves, so poll (real time) until both I/O task clones are
    // gone — leaving only the external `probe`. PRE-fix this never happens (the
    // writer stays parked) and the loop times out.
    let deadline = std::time::Instant::now() + Duration::from_secs(5);
    loop {
        let count = probe.state_strong_count();
        if count == 1 {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "both I/O task transport clones must be released on the death edge, \
             leaving only the external probe (strong_count == 1), but it stayed at \
             {count} — the writer clone leaked (pre-fix behavior)"
        );
        tokio::time::sleep(Duration::from_millis(25)).await;
    }

    // The connection is still held (never dropped) and the writer channel was
    // never closed: the release happened purely on the death edge.
    assert!(
        !conn.is_alive(),
        "the connection still held alive must report not-alive after the death edge"
    );
}

// ===========================================================================
// ChunkedTransport (byte-level) — Phase A: carries a normal handshake, including
// trickled byte delivery across idle ReadTimeouts.
// ===========================================================================

/// The byte-level transport carries a full handshake when frames are delivered
/// whole.
#[tokio::test(start_paused = true)]
async fn chunked_transport_completes_handshake_whole_frames() {
    let conn = PersistentConnection::new_with_features(
        ChunkedTransport::new(DeviceProfile::android_16()),
        Some(ephemeral_key_path()),
        DeviceFeatureSet::default(),
    )
    .await
    .expect("byte-level transport must carry a normal handshake");
    assert!(
        conn.delayed_ack_negotiated(),
        "byte-level path must negotiate the same windowed mode as the frame path"
    );
}

/// The byte-level transport reassembles a frame delivered a few bytes at a time
/// (each sub-frame read returns the idle `ReadTimeout`), proving the shared
/// `FrameReadBuffer` keeps the stream aligned across idle-timeout boundaries —
/// the consumer-side cancel-safety property (full fault matrix in Phase B).
#[tokio::test(start_paused = true)]
async fn chunked_transport_reassembles_frame_split_across_idle_reads() {
    let conn = PersistentConnection::new_with_features(
        ChunkedTransport::new(DeviceProfile::android_16()).with_read_chunk(7),
        Some(ephemeral_key_path()),
        DeviceFeatureSet::default(),
    )
    .await
    .expect("a frame trickled 7 bytes per read must reassemble into a clean handshake");
    assert!(
        conn.is_alive(),
        "connection is alive after a trickled handshake"
    );
}

/// Directly exercise the byte-level read contract: a CNXN banner delivered in
/// 7-byte chunks yields `ReadTimeout` until the whole frame is present, then the
/// frame — never a desync.
#[tokio::test(start_paused = true)]
async fn chunked_transport_read_trickle_returns_timeout_until_frame_complete() {
    let mut transport = ChunkedTransport::new(DeviceProfile::android_16()).with_read_chunk(7);
    // Provoke the device to enqueue its CNXN banner.
    let cnxn =
        ADBTransportMessage::try_new(MessageCommand::Cnxn, 0x0100_0001, 1_048_576, b"host::")
            .expect("build CNXN");
    transport
        .write_message(cnxn)
        .await
        .expect("write drives the device reaction");

    // The banner frame is > 7 bytes, so the first reads time out (idle) until the
    // whole frame has trickled in.
    let mut timeouts = 0;
    let frame = loop {
        match transport
            .read_message_with_timeout(Duration::from_millis(1))
            .await
        {
            Ok(frame) => break frame,
            Err(RustADBError::ReadTimeout) => {
                timeouts += 1;
                assert!(timeouts < 1000, "must complete the frame in bounded reads");
            }
            Err(e) => panic!("only ReadTimeout is allowed mid-frame, got {e:?}"),
        }
    };
    assert_eq!(
        frame.header().command(),
        MessageCommand::Cnxn,
        "the trickled bytes must reassemble into the CNXN banner frame"
    );
    assert!(
        timeouts > 0,
        "a frame larger than the 7-byte chunk MUST take several reads (idle timeouts) to assemble"
    );
}
