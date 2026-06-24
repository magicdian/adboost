//! Phase B session suite: OPEN / WRTE / OKAY / CLSE, flow control (classic vs
//! windowed), teardown, and the byte-level [`ChunkedTransport`] fault scenarios —
//! all driven end-to-end through the live `PersistentConnection` reader/writer
//! loops over a [`SimulatedDevice`]. Named regression locks: B3a (early-CLSE
//! fast-fail), B8 (half-open `is_alive`), B4/B5/B7/B9 (byte-level cancel-safety).
//!
//! Run under `cargo test --features usb`. No hardware, no network.

use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use super::{ChunkedTransport, DeviceProfile, OpenResponse, Scenario, SimulatedDevice};
use crate::RustADBError;
use crate::message_devices::usb::persistent::PersistentConnection;
use crate::models::{ADBLocalCommand, DeviceFeatureSet};

fn ephemeral_key_path() -> std::path::PathBuf {
    std::path::PathBuf::from("/nonexistent/adboost-sim-tests/this-key-does-not-exist")
}

/// Connect over a frame-level simulated device with the given scenario, default
/// (windowing-on) feature set.
async fn connect(
    profile: DeviceProfile,
    scenario: Scenario,
) -> crate::Result<PersistentConnection<SimulatedDevice>> {
    PersistentConnection::new_with_features(
        SimulatedDevice::with_scenario(profile, scenario),
        Some(ephemeral_key_path()),
        DeviceFeatureSet::default(),
    )
    .await
}

// ===========================================================================
// OPEN handshake (OPEN-1, OPEN-2, OPEN-3) + double-OKAY framing
// ===========================================================================

/// OPEN-1: a healthy windowed device accepts an OPEN; `open_session` returns a
/// session whose `remote_id` is the device's local id from the OKAY.
#[tokio::test(start_paused = true)]
async fn open_session_succeeds_and_sets_remote_id() {
    let conn = connect(DeviceProfile::android_16(), Scenario::healthy())
        .await
        .expect("handshake completes");
    let session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN must be accepted with an OKAY");
    assert_ne!(
        session.remote_id(),
        0,
        "remote_id must be seeded from the device's OKAY arg0 (its local id)"
    );
}

/// OPEN double-OKAY: a device that replies OKAY twice to one OPEN must not break
/// the open handshake (the extra OKAY is a credit/ready poke).
#[tokio::test(start_paused = true)]
async fn open_session_tolerates_double_okay() {
    let conn = connect(
        DeviceProfile::android_16(),
        Scenario::healthy().with_open_response(OpenResponse::AcceptDoubleOkay),
    )
    .await
    .expect("handshake completes");
    conn.open_session(&ADBLocalCommand::Shell)
        .await
        .expect("a double-OKAY reply to OPEN must still open the session cleanly");
}

/// OPEN-2 / B3a REGRESSION: a device that rejects the OPEN with
/// `CLSE(0, host_local_id)` on the data channel must make `open_session`
/// **fast-fail**, NOT hang for the 10 s OPEN-response timeout. Under
/// `start_paused`, a hang would still resolve instantly in virtual time, so we
/// assert the error is the CLSE-rejection (fast-fail) shape, not a timeout.
#[tokio::test(start_paused = true)]
async fn open_session_rejected_with_clse_fails_fast() {
    let conn = connect(
        DeviceProfile::android_16(),
        Scenario::healthy().with_open_response(OpenResponse::RejectWithClse),
    )
    .await
    .expect("handshake completes");
    let err = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .err()
        .expect("a CLSE-rejected OPEN must fail, not return a session");
    match err {
        RustADBError::ADBRequestFailed(msg) => assert!(
            msg.contains("CLSE") || msg.to_lowercase().contains("reject"),
            "B3a: OPEN rejection must fail fast with a CLSE-rejection error, not a \
             10s timeout; got ADBRequestFailed({msg:?})"
        ),
        other => panic!("expected a CLSE-rejection ADBRequestFailed, got {other:?}"),
    }
}

/// OPEN-3: a device that silently ignores the OPEN makes `open_session` surface
/// the OPEN-response timeout (bounded, not an infinite hang).
#[tokio::test(start_paused = true)]
async fn open_session_times_out_when_ignored() {
    let conn = connect(
        DeviceProfile::android_16(),
        Scenario::healthy().with_open_response(OpenResponse::Ignore),
    )
    .await
    .expect("handshake completes");
    let err = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .err()
        .expect("an ignored OPEN must time out, not return a session");
    assert!(
        matches!(err, RustADBError::ADBRequestFailed(_)),
        "an ignored OPEN must surface a bounded ADBRequestFailed timeout, got {err:?}"
    );
}

// ===========================================================================
// Session byte stream: WRTE delivery + OKAY, echo read, CLSE -> EOF (SES-1, SES-4)
// ===========================================================================

/// SES-1/SES-4: write a payload, read the device's echo, then observe EOF on the
/// device's CLSE. Exercises the full WRTE→OKAY→WRTE→CLSE round trip over the live
/// loops with windowed flow control.
#[tokio::test(start_paused = true)]
async fn session_write_read_echo_then_eof_on_close() {
    let conn = connect(
        DeviceProfile::android_16(),
        Scenario::healthy()
            .with_echo_bytes(64)
            .with_close_after_first_write(),
    )
    .await
    .expect("handshake completes");
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN accepted");

    session
        .write_all(b"hello device")
        .await
        .expect("windowed write must succeed (window granted by the device OKAY)");

    let mut buf = [0_u8; 64];
    let n = session
        .read(&mut buf)
        .await
        .expect("must read the device's echo WRTE");
    assert_eq!(
        &buf[..n],
        b"hello device",
        "the device echoed back exactly what we wrote"
    );

    // The device sent CLSE after the first write → the next read is EOF (0 bytes).
    let eof = session
        .read(&mut buf)
        .await
        .expect("read after CLSE is EOF, not error");
    assert_eq!(
        eof, 0,
        "SES-4: a device CLSE must surface as EOF (0-byte read)"
    );
}

/// A second, independent session can be opened on the same connection — the
/// reader demultiplexes by local id (groundwork for multi-session interleave).
#[tokio::test(start_paused = true)]
async fn two_sequential_sessions_on_one_connection() {
    let conn = connect(DeviceProfile::android_16(), Scenario::healthy())
        .await
        .expect("handshake completes");
    let s1 = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("first opens");
    let id1 = s1.local_id();
    drop(s1);
    let s2 = conn
        .open_session(&ADBLocalCommand::Sync)
        .await
        .expect("second opens after the first is dropped");
    assert_ne!(id1, s2.local_id(), "each session gets a distinct local id");
}

// ===========================================================================
// Flow control: classic vs windowed write both succeed (FC-1, FC-2)
// ===========================================================================

/// FC-1: over a classic (non-windowed) connection, a write still completes
/// (stop-and-wait: the device's empty-payload OKAY acks each WRTE). Driven by an
/// Android-11 device, which negotiates classic.
#[tokio::test(start_paused = true)]
async fn classic_flow_control_write_succeeds() {
    let conn = connect(DeviceProfile::android_11(), Scenario::healthy())
        .await
        .expect("handshake completes");
    assert!(
        !conn.delayed_ack_negotiated(),
        "Android-11 must negotiate classic flow control"
    );
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN accepted in classic mode");
    session
        .write_all(b"classic write")
        .await
        .expect("a classic stop-and-wait write must complete on the device OKAY");
}

/// FC-2: over a windowed connection, the device's 32 MiB grant lets a write
/// proceed without blocking.
#[tokio::test(start_paused = true)]
async fn windowed_flow_control_write_succeeds() {
    let conn = connect(DeviceProfile::android_16(), Scenario::healthy())
        .await
        .expect("handshake completes");
    assert!(
        conn.delayed_ack_negotiated(),
        "Android-16 must negotiate windowed flow control"
    );
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN accepted in windowed mode");
    session
        .write_all(b"windowed write")
        .await
        .expect("a windowed write must proceed on the granted 32 MiB window");
}

// ===========================================================================
// Liveness / teardown: B8 (half-open is_alive) + graceful close
// ===========================================================================

/// B8 REGRESSION: when the device's reader half dies (adbd closed the
/// connection) after the handshake, `is_alive()` must report false — a half-open
/// connection (one task finished) must never be considered reusable.
#[tokio::test]
async fn half_open_connection_reports_not_alive() {
    let conn = connect(
        DeviceProfile::android_16(),
        Scenario::healthy().with_death_after_reads(1),
    )
    .await
    .expect("handshake completes before the reader dies");

    tokio::time::timeout(Duration::from_secs(5), conn.wait_closed())
        .await
        .expect("the reader's fatal death must resolve wait_closed");
    assert!(
        !conn.is_alive(),
        "B8: a connection with a finished reader task (half-open) must report not-alive"
    );
}

/// A graceful `shutdown()` on a live connection completes without error: it
/// flushes exactly one connection-level CLSE to the (alive) device. The device
/// receives it as a session-less CLSE and the call returns cleanly. (`shutdown`
/// does not itself abort the I/O tasks — that is `Drop`'s job — so we assert the
/// flush completes, not that the connection dies.)
#[tokio::test(start_paused = true)]
async fn graceful_shutdown_flushes_without_error() {
    let conn = connect(DeviceProfile::android_16(), Scenario::healthy())
        .await
        .expect("handshake completes");
    // Open a session so the graceful close has live state to retire.
    let session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN accepted");
    drop(session);
    // Completes without panicking/hanging: the one connection-level CLSE is
    // flushed with ack while the writer is still alive.
    tokio::time::timeout(Duration::from_secs(5), conn.shutdown())
        .await
        .expect("graceful shutdown must flush the connection CLSE and return");
}

// ===========================================================================
// ChunkedTransport byte-level fault scenarios (B4, B5, B7, B9)
// ===========================================================================

/// Build a windowed connection over the byte-level transport with `scenario`.
async fn connect_chunked(
    scenario: Scenario,
    read_chunk: Option<usize>,
) -> crate::Result<PersistentConnection<ChunkedTransport>> {
    let mut transport = ChunkedTransport::with_scenario(DeviceProfile::android_16(), scenario);
    if let Some(n) = read_chunk {
        transport = transport.with_read_chunk(n);
    }
    PersistentConnection::new_with_features(
        transport,
        Some(ephemeral_key_path()),
        DeviceFeatureSet::default(),
    )
    .await
}

/// B4: a device→host frame trickled a few bytes per read (each sub-frame read
/// returning the idle `ReadTimeout`) must reassemble intact and keep the session
/// usable — the consumer-side cancel-safety property over the live reader loop.
#[tokio::test(start_paused = true)]
async fn chunked_session_survives_frame_split_across_idle_reads() {
    let conn = connect_chunked(
        Scenario::healthy().with_echo_bytes(32),
        Some(5), // trickle 5 bytes per read
    )
    .await
    .expect("handshake reassembles under trickled byte delivery");
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN reassembles across idle reads");
    session
        .write_all(b"chunked echo test")
        .await
        .expect("write");
    let mut buf = [0_u8; 32];
    let n = session.read(&mut buf).await.expect("echo reassembles");
    assert_eq!(
        &buf[..n],
        b"chunked echo test",
        "B4: an echoed frame trickled across idle ReadTimeouts must reassemble intact"
    );
}

/// B5: several device frames coalesced into one bulk-IN read (over-delivery) must
/// be split back into individual frames by the reassembly buffer — the handshake
/// + a session still work when the device's OKAY/echo frames arrive coalesced.
#[tokio::test(start_paused = true)]
async fn chunked_session_survives_coalesced_frames() {
    let conn = connect_chunked(
        Scenario::healthy()
            .with_echo_bytes(16)
            .with_coalesced_frames(3),
        None,
    )
    .await
    .expect("handshake survives coalesced frame delivery");
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN survives coalesced OKAY delivery");
    session.write_all(b"coalesced").await.expect("write");
    let mut buf = [0_u8; 16];
    let n = session
        .read(&mut buf)
        .await
        .expect("echo splits out of the coalesced read");
    assert_eq!(
        &buf[..n],
        b"coalesced",
        "B5: coalesced device frames must split cleanly into individual frames"
    );
}

/// B7: a mid-frame write truncation (bytes committed, then failure) must be
/// FATAL — the persistent writer poisons the connection rather than
/// warn-and-continue (a partial frame on the wire desyncs the peer). We inject
/// the fault on a session WRTE and assert the connection dies.
#[tokio::test]
async fn chunked_mid_frame_write_truncation_is_fatal() {
    // The handshake performs a few writes (CNXN, ready-OKAY, OPEN, ready-OKAY);
    // target a write index comfortably past them so the fault lands on the
    // session's data WRTE. The writer task turns a fatal write error into a
    // connection teardown.
    let conn = connect_chunked(
        Scenario::healthy().with_write_fault(50, 8), // 8 bytes committed -> fatal
        None,
    )
    .await
    .expect("handshake completes before the far-future write fault");
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN completes");

    // Keep writing until the poisoned writer tears the connection down. Each
    // write goes through the writer task; the faulted one makes it exit, after
    // which the connection is not alive.
    for _ in 0..100 {
        let _ = session.write_all(b"data").await;
        if !conn.is_alive() {
            break;
        }
        tokio::task::yield_now().await;
    }
    tokio::time::timeout(Duration::from_secs(5), conn.wait_closed())
        .await
        .expect("B7: a mid-frame write truncation must be fatal (connection dies)");
    assert!(
        !conn.is_alive(),
        "B7: a partial-frame write must poison the connection (not warn-and-continue)"
    );
}

/// B-recv REGRESSION: a device that replies to a SYNC `RECV` with a too-short
/// frame (fewer than the 8-byte SYNC header, then closes) must surface a clean
/// error from `SyncSession::pull`, NOT panic on an out-of-bounds slice. The
/// post-fix `read_frame_header` uses `read_exact`, so a truncated frame is a
/// graceful `UnexpectedEof`; this locks that against regression through the sim.
#[tokio::test(start_paused = true)]
async fn sync_pull_short_frame_errors_without_panic() {
    let conn = connect(
        DeviceProfile::android_16(),
        // Reply to the first session WRTE (the RECV request) with a 3-byte frame
        // (shorter than the 8-byte SYNC header), then close the stream.
        Scenario::healthy()
            .with_first_write_reply(vec![0x01, 0x02, 0x03])
            .with_close_after_first_write(),
    )
    .await
    .expect("handshake completes");
    let mut sync = conn.open_sync_session().await.expect("sync session opens");
    let mut sink: Vec<u8> = Vec::new();
    let result = sync.pull("/some/remote/path", &mut sink).await;
    assert!(
        result.is_err(),
        "B-recv: a too-short SYNC reply frame must surface a clean error (no panic)"
    );
}

/// B9: a write-start backpressure `WriteTimeout` (nothing committed) is
/// RECOVERABLE — the writer keeps looping and the connection stays alive. We
/// inject a 0-byte-committed fault and assert the connection survives.
#[tokio::test(start_paused = true)]
async fn chunked_write_start_backpressure_is_recoverable() {
    let conn = connect_chunked(
        Scenario::healthy().with_write_fault(50, 0), // 0 bytes committed -> recoverable
        None,
    )
    .await
    .expect("handshake completes");
    let mut session = conn
        .open_session(&ADBLocalCommand::Shell)
        .await
        .expect("OPEN completes");
    // The write that hits the backpressure fault may report an error to the
    // caller, but the CONNECTION must remain alive (frame-atomic Scheme B: a
    // 0-byte-committed WriteTimeout is recoverable, not a teardown).
    let _ = session.write_all(b"backpressure").await;
    assert!(
        conn.is_alive(),
        "B9: a 0-byte-committed WriteTimeout (backpressure) must NOT tear down the connection"
    );
}
