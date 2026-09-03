//! Raw smartsocket wire-protocol cases against the in-process adboost server.
//!
//! [`super::parity`] drives the frontend with the *official `adb` CLI*; these
//! cases speak the host protocol directly over TCP — the way non-CLI clients
//! such as Android Studio's adblib consume an adb server. That distinction is
//! the point: the reported AS blank-device-list bug lived in exactly this gap
//! (adblib's `SessionDeviceTracker` sends `host:track-devices-l`, which no
//! `adb` CLI invocation exercises).

use std::net::SocketAddrV4;
use std::time::Duration;

use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpStream;

use super::report::Outcome;

/// Overall bound for one request/reply exchange (connect + request + frame).
const EXCHANGE_TIMEOUT: Duration = Duration::from_secs(10);

/// Send one framed smartsocket request, read the 4-byte status plus one
/// `%04x`-framed payload, then drop the connection. The helper for one-shot
/// host queries (`host:devices`, `host:devices-l`, …); streaming services use
/// [`TrackStream`].
async fn one_shot_frame(addr: SocketAddrV4, service: &str) -> Result<(String, String), String> {
    let mut stream = connect_and_send(addr, service).await?;
    let status = read_status(&mut stream).await?;
    let payload = read_frame(&mut stream).await?;
    Ok((status, payload))
}

/// Connect to the in-process server and send one framed service request.
async fn connect_and_send(addr: SocketAddrV4, service: &str) -> Result<TcpStream, String> {
    let connect = async {
        let mut stream = TcpStream::connect(addr)
            .await
            .map_err(|e| format!("connect to in-process server: {e}"))?;
        let framed = format!("{:04x}{service}", service.len());
        stream
            .write_all(framed.as_bytes())
            .await
            .map_err(|e| format!("write {service}: {e}"))?;
        stream.flush().await.map_err(|e| format!("flush: {e}"))?;
        Ok(stream)
    };
    tokio::time::timeout(EXCHANGE_TIMEOUT, connect)
        .await
        .map_err(|_| format!("connect/send {service} timed out"))?
}

/// Read the 4-byte `OKAY`/`FAIL` status.
async fn read_status(stream: &mut TcpStream) -> Result<String, String> {
    let mut buf = [0u8; 4];
    stream
        .read_exact(&mut buf)
        .await
        .map_err(|e| format!("read status: {e}"))?;
    Ok(String::from_utf8_lossy(&buf).to_string())
}

/// Read one `%04x`-framed payload (the shape every `track-devices*` snapshot
/// and host data-query reply uses).
async fn read_frame(stream: &mut TcpStream) -> Result<String, String> {
    let mut len_buf = [0u8; 4];
    stream
        .read_exact(&mut len_buf)
        .await
        .map_err(|e| format!("read frame length: {e}"))?;
    let len = usize::from_str_radix(
        std::str::from_utf8(&len_buf).map_err(|e| format!("frame length not UTF-8: {e}"))?,
        16,
    )
    .map_err(|e| format!("frame length not hex: {e}"))?;
    let mut payload = vec![0u8; len];
    stream
        .read_exact(&mut payload)
        .await
        .map_err(|e| format!("read frame payload: {e}"))?;
    Ok(String::from_utf8_lossy(&payload).to_string())
}

/// An open streaming `host:track-devices*` connection: the OKAY has been
/// consumed; each [`Self::next_snapshot`] reads one pushed device-list
/// snapshot, bounded by a timeout so a silent server fails the case instead of
/// hanging the harness.
pub(super) struct TrackStream {
    stream: TcpStream,
}

impl TrackStream {
    /// Open a track-devices stream and consume its accept `OKAY`.
    pub(super) async fn open(addr: SocketAddrV4, service: &str) -> Result<Self, String> {
        let mut stream = connect_and_send(addr, service).await?;
        let status = read_status(&mut stream).await?;
        if status != "OKAY" {
            // Read the FAIL reason for the failure message.
            let reason = read_frame(&mut stream).await.unwrap_or_default();
            return Err(format!("{service} replied {status}: {reason}"));
        }
        Ok(Self { stream })
    }

    /// Read the next pushed snapshot, bounded by `timeout`.
    pub(super) async fn next_snapshot(&mut self, timeout: Duration) -> Result<String, String> {
        tokio::time::timeout(timeout, read_frame(&mut self.stream))
            .await
            .map_err(|_| "timed out waiting for the next track-devices snapshot".to_string())?
    }
}

/// Whether a device-list snapshot contains a line for `serial` (line-prefix
/// match, so one serial that is a prefix of another cannot false-positive).
/// Shared with [`super::interactive`]'s hotplug case.
pub(super) fn snapshot_has_serial(snapshot: &str, serial: &str) -> bool {
    let prefix = format!("{serial}\t");
    snapshot.lines().any(|l| l.starts_with(&prefix))
}

/// The device-list streaming family, end-to-end on real hardware:
///
/// - `host:track-devices-l` must reply OKAY and its **first snapshot** must be
///   the LONG format (state + `transport_id`) and **byte-equal** the one-shot
///   `host:devices-l` reply — the shared-renderer invariant adblib's
///   `DeviceListTextParser(LONG_FORMAT)` relies on. This is the runtime guard
///   for the reported Android Studio blank-device-list regression (adblib has
///   no fallback when the service is missing).
/// - the legacy `host:track-devices` short stream must byte-equal
///   `host:devices` (regression lock for the old ddmlib / CLI path).
///
/// Non-destructive (read-only queries), so it runs in the automated
/// through-server phase, once per serial.
pub(super) async fn case_track_devices_family(addr: SocketAddrV4, serial: &str) -> Outcome {
    // --- long format: stream vs one-shot ------------------------------------
    let mut tracker = match TrackStream::open(addr, "host:track-devices-l").await {
        Ok(t) => t,
        Err(e) => {
            return Outcome::Failed(format!(
                "REGRESSION: host:track-devices-l could not be opened — the arm Android \
                 Studio's adblib depends on is missing: {e}"
            ));
        }
    };
    let streamed_long = match tracker.next_snapshot(EXCHANGE_TIMEOUT).await {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(format!("track-devices-l first snapshot: {e}")),
    };
    if !snapshot_has_serial(&streamed_long, serial) {
        return Outcome::Failed(format!(
            "track-devices-l snapshot lacks a line for {serial}: {streamed_long:?}"
        ));
    }
    if !streamed_long.contains("transport_id:") {
        return Outcome::Failed(format!(
            "track-devices-l snapshot is not the LONG format (no transport_id): \
             {streamed_long:?}"
        ));
    }
    let (status, devices_l) = match one_shot_frame(addr, "host:devices-l").await {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(format!("host:devices-l: {e}")),
    };
    if status != "OKAY" || streamed_long != devices_l {
        return Outcome::Failed(format!(
            "track-devices-l first snapshot must byte-equal host:devices-l \
             (status {status}): streamed={streamed_long:?} one-shot={devices_l:?}"
        ));
    }

    // --- legacy short format: stream vs one-shot (regression) ----------------
    let mut tracker = match TrackStream::open(addr, "host:track-devices").await {
        Ok(t) => t,
        Err(e) => return Outcome::Failed(format!("host:track-devices: {e}")),
    };
    let streamed_short = match tracker.next_snapshot(EXCHANGE_TIMEOUT).await {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(format!("track-devices first snapshot: {e}")),
    };
    let (status, devices) = match one_shot_frame(addr, "host:devices").await {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(format!("host:devices: {e}")),
    };
    if status != "OKAY" || streamed_short != devices {
        return Outcome::Failed(format!(
            "track-devices snapshot must byte-equal host:devices (status {status}): \
             streamed={streamed_short:?} one-shot={devices:?}"
        ));
    }
    Outcome::Passed
}
