//! The bundled, zero-config default device backend (USB + TCP/IP).
//!
//! [`DefaultDeviceBackend`] is the default [`DeviceBackend`]: it enumerates ADB
//! USB devices, tracks `host:connect`ed TCP/IP devices, opens local services
//! over a [`PersistentConnection`](crate::usb::PersistentConnection), and
//! reports device-set changes via nusb
//! hotplug. Both paths are thin wrappers — all device-side transport,
//! multiplexing, and flow control come from the (now transport-generic)
//! persistent connection; this type only maps serials to connections and caches
//! them. USB serials resolve to a `PersistentConnection<USBTransport>`
//! ([`PersistentUsbConnection`]); `host:connect`ed TCP serials resolve to a
//! `PersistentConnection<TcpTransport>`, so a client's `shell:`/`tcp:`/`sync:`/
//! `shell,v2` is bridged through to a TCP/IP device exactly as to a USB one.
//!
//! [`UsbDeviceBackend`] remains as a deprecated type alias for source
//! compatibility.

use std::collections::{HashMap, HashSet};
use std::future::Future;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};
use std::time::{Duration, Instant};

use futures_util::StreamExt;
use tokio::sync::{Mutex, broadcast, mpsc};

use super::backend::{
    BackendCapabilities, DeviceBackend, DeviceEntry, LifecycleEvent, TransportKind,
};
use crate::models::{ADBLocalCommand, DeviceFeatureSet};
use crate::usb::{
    MultiplexedSession, PersistentTcpConnection, PersistentUsbConnection, ReverseEngine,
    ReversePolicy, ShellV2Session, SyncSession, find_all_connected_adb_devices,
};
use crate::{Result, RustADBError};

/// Channel depth for the `subscribe_changes` snapshot stream. Small: only the
/// latest snapshot matters, and the consumer (one `host:track-devices` client)
/// drains promptly.
const CHANGES_CHANNEL_SIZE: usize = 8;

/// Channel depth for the internal device-lifecycle stream
/// ([`DeviceBackend::subscribe_lifecycle`]). Disconnect events are infrequent
/// (a human unplugging a cable, an `adb disconnect`), so a small buffer amply
/// absorbs a burst (e.g. a hub losing power drops several serials at once)
/// before the frontend drains it.
const LIFECYCLE_CHANNEL_SIZE: usize = 32;

/// The default adbd-over-TCP port (`adb connect <host>` with no `:port`).
const DEFAULT_ADB_TCP_PORT: u16 = 5555;

/// Wall-clock budget for [`DefaultDeviceBackend::get_or_open`]'s open/first-OPEN
/// retry. Sized to cover a USB re-enumeration window (adbd restart from
/// `root:`/`unroot:`/`tcpip:`/`usb:`/reboot, or a replug) — the device can be
/// briefly absent from enumeration AND, once back, not yet ready for the first
/// transfer. A truly-absent device fails fast within this bound rather than
/// hanging. This is the backend analogue of the selftest's `open_device_with_retry`
/// (which it replaces for the server `adb root` reconnect path).
const OPEN_RETRY_BUDGET: Duration = Duration::from_secs(10);

/// Poll interval between [`OPEN_RETRY_BUDGET`] open attempts. Coarser than
/// `do_connect`'s 100 ms handshake settle because this layer polls a slower
/// signal (the device returning to enumeration), not a single in-flight transfer.
const OPEN_RETRY_POLL: Duration = Duration::from_millis(500);

/// Retry an async fallible op within a wall-clock `budget`, sleeping `poll`
/// between attempts, but only while the error is deemed retryable. Returns `Ok`
/// on the first success, or the last error once the budget elapses (so a
/// truly-absent / permanently-failing op fails fast within ~`budget` instead of
/// hanging). A non-retryable error returns immediately.
///
/// Pure over its inputs (op + predicate + clock) so it is unit-testable with a
/// closure that fails N times then succeeds (and one that always fails), with no
/// USB binding — see the tests at the bottom of this file.
async fn retry_within<T, F, Fut>(
    budget: Duration,
    poll: Duration,
    is_retryable: impl Fn(&RustADBError) -> bool,
    mut op: F,
) -> Result<T>
where
    F: FnMut() -> Fut,
    Fut: Future<Output = Result<T>>,
{
    let deadline = Instant::now() + budget;
    loop {
        match op().await {
            Ok(value) => return Ok(value),
            // Stop on a non-retryable error or once the budget is spent: a
            // genuinely-absent device or a real fault must fail fast, never hang.
            Err(e) if !is_retryable(&e) || Instant::now() >= deadline => return Err(e),
            Err(_) => tokio::time::sleep(poll).await,
        }
    }
}

/// Whether an open-time error is worth retrying within [`OPEN_RETRY_BUDGET`].
///
/// **This layer OWNS re-enumeration recovery.** Each [`Self::get_or_open`]
/// `retry_within` poll calls `new_from_serial` → `USBTransport::new_by_serial` →
/// a *fresh* `connect()` → fresh bulk endpoints. When `adb root`/`unroot`
/// restarts adbd, the device re-enumerates under a NEW `IOKit` registry id and the
/// old transport's endpoints are permanently dead — so rebuilding the transport
/// is the ONLY thing that recovers it. The back-to-back `adb root; adb unroot`
/// real-hardware trace proved this: 15 in-place CNXN retries on the dead handle
/// ALL failed `device disconnected`, then the FIRST reopen of a fresh transport
/// succeeded immediately. The inner `do_connect` retry is therefore only a
/// same-handle blip catcher (tiny `CONNECT_TRANSIENT_MAX_ATTEMPTS`);
/// re-enumeration recovery is HERE, at the reopen layer. `open_sync_session` /
/// `open_shell_v2` inherit this via bare `get_or_open`; `open_local_service`
/// keeps `open_session_with_reopen` for the first-OPEN race on top.
///
/// So the predicate is a **reopen-window FAMILY** classifier, not a code list:
/// any `TransferError` EXCEPT the structurally-fatal ones is a re-enumeration
/// transient, because the outer retry rebuilds the transport each poll and the
/// 10 s wall clock keeps a real unplug/fault failing fast. Classifying by VARIANT
/// FAMILY (not by enumerating `IOKit` codes — `0xe00002ed` `NotResponding`,
/// `0xe00002d8` `NotReady`, … all land in `Unknown(_)`) ends the whack-a-mole of
/// adding one constant per newly-observed code.
///
/// Retryable (rebuild the transport, bounded by the 10 s wall clock):
/// - `UsbTransferError(Stall | Disconnected | Unknown(_))` — every re-enumeration
///   transient (endpoint stall while adbd restarts, `NoDevice` during the gap,
///   and any OS-specific code in the `Unknown` catch-all).
/// - [`RustADBError::ADBRequestFailed`] — a CNXN-exhausted handshake out of
///   `do_connect`: the explicit "the old transport is dead, reopen" signal. (At
///   the `new_from_serial` open layer the realistic `ADBRequestFailed` IS the
///   CNXN-exhaustion case — other `ADBRequestFailed`s arise later, on an
///   established session, not during open. Matching it broadly is acceptable
///   because the 10 s wall clock bounds it; a precise message-substring match was
///   rejected as fragile.)
/// - [`RustADBError::DeviceNotFound`] — the device is *momentarily absent from
///   enumeration* (`USBTransport::new_by_serial` returns this), which the
///   handshake layer cannot see because the transport is not even built yet.
///
/// NOT retried (structurally fatal — must fail fast, never masked by the budget):
/// - `UsbTransferError(InvalidArgument)` — a programming error / unsupported
///   request, never a re-enumeration blip.
/// - `UsbTransferError(Fault)` — a hardware issue / protocol violation, not a
///   re-enumeration blip (conservative; the wall clock would bound it either way).
/// - [`RustADBError::DeviceBusy`] — another process holds the single USB claim;
///   waiting will not clear it.
///
/// **This REVERSES the previous task's anti-amplification decoupling** (which
/// excluded `Stall`/`ADBRequestFailed` here so an enlarged inner CNXN budget would
/// not multiply with this outer one). The trace showed that decoupling was the
/// bug: it BLOCKED the outer layer from recovering re-enumeration. The inversion
/// is now SAFE because the inner transient arm is a small constant
/// (`persistent::CONNECT_TRANSIENT_MAX_ATTEMPTS`, a few hundred ms), so the total
/// worst-case ≈ this outer wall clock (10 s), NOT a product.
fn is_retryable_open_error(e: &RustADBError) -> bool {
    use nusb::transfer::TransferError;
    matches!(
        e,
        RustADBError::UsbTransferError(
            TransferError::Stall | TransferError::Disconnected | TransferError::Unknown(_)
        ) | RustADBError::ADBRequestFailed(_)
            | RustADBError::DeviceNotFound(_)
    )
}

/// Default [`DeviceBackend`]: serial-keyed [`PersistentUsbConnection`] cache +
/// nusb hotplug change stream for USB, plus a registry of `host:connect`ed
/// TCP/IP devices. No custom discovery/relay/auth — inject your own
/// `DeviceBackend` for that.
#[derive(Default)]
pub struct DefaultDeviceBackend {
    /// One persistent (single-claim) connection per device serial, opened
    /// lazily on first `open_local_service` and reused across sessions.
    conns: Mutex<HashMap<String, Arc<PersistentUsbConnection>>>,
    /// Optional ADB private-key path passed to every connection (None → default
    /// `~/.android/adbkey`, as resolved by `PersistentUsbConnection`).
    private_key_path: Option<PathBuf>,
    /// Per-serial reverse engine (rules + lazy inbound-open pump). Created on the
    /// first `reverse:forward:` for a serial; absent for devices never using
    /// reverse.
    reverse: Mutex<HashMap<String, Arc<ReverseEngine>>>,
    /// Security policy for accepting device-initiated reverse opens.
    reverse_policy: ReversePolicy,
    /// `host:connect`ed TCP/IP devices, keyed by their normalized
    /// `<ip>:<port>` serial. Each value is a persistent multiplexed connection
    /// (the TCP analogue of the USB `conns` pool) that keeps the authenticated
    /// connection alive so the device stays listed AND lets local services
    /// (`shell:` / `tcp:` / `sync:` / `shell,v2`) be bridged through to the
    /// device, exactly as on USB.
    ///
    /// Wrapped in an `Arc` so the `host:track-devices` hotplug watcher task can
    /// hold a clone and fold TCP devices into each snapshot it pushes.
    tcp_devices: Arc<Mutex<HashMap<String, Arc<PersistentTcpConnection>>>>,
    /// Internal device-lifecycle event hub (see
    /// [`DeviceBackend::subscribe_lifecycle`]). A `broadcast` channel so the
    /// single long-lived USB hotplug watcher and the synchronous TCP
    /// `disconnect` path can both publish [`LifecycleEvent`]s, and every
    /// `subscribe_lifecycle` caller gets its own receiver. Lazily initialized on
    /// first subscription so a backend that never serves pays nothing; the
    /// hotplug-diff watcher task is spawned exactly once alongside it.
    lifecycle: OnceLock<broadcast::Sender<LifecycleEvent>>,
    /// Guards one-time spawn of the USB hotplug-diff watcher that feeds
    /// `lifecycle`. Set the first time [`Self::lifecycle_tx`] initializes the hub.
    lifecycle_watch_started: AtomicBool,
}

/// Deprecated former name of [`DefaultDeviceBackend`]. The default backend now
/// also tracks TCP/IP devices, so the `Usb`-specific name no longer fits; this
/// alias keeps existing `use adboost::server::UsbDeviceBackend` compiling.
#[deprecated(
    since = "3.2.2",
    note = "renamed to DefaultDeviceBackend (it now also tracks TCP/IP devices)"
)]
pub type UsbDeviceBackend = DefaultDeviceBackend;

impl DefaultDeviceBackend {
    /// A backend using the default ADB key location and the default reverse
    /// policy ([`ReversePolicy::RejectUnconfigured`]).
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A backend using a specific ADB private-key path for every device.
    #[must_use]
    pub fn with_private_key(private_key_path: PathBuf) -> Self {
        Self {
            private_key_path: Some(private_key_path),
            ..Default::default()
        }
    }

    /// Set the reverse security policy (default [`ReversePolicy::RejectUnconfigured`]).
    #[must_use]
    pub fn with_reverse_policy(mut self, policy: ReversePolicy) -> Self {
        self.reverse_policy = policy;
        self
    }

    /// Get (creating on first use) the [`ReverseEngine`] for `serial`, bound to
    /// the device's persistent connection. The engine starts its inbound-open
    /// pump lazily on its first `open`.
    async fn reverse_engine(&self, serial: &str) -> Result<Arc<ReverseEngine>> {
        let conn = self.get_or_open(serial).await?;
        let engine = {
            let mut map = self.reverse.lock().await;
            Arc::clone(
                map.entry(serial.to_owned())
                    .or_insert_with(|| ReverseEngine::new(conn, self.reverse_policy.clone())),
            )
        };
        Ok(engine)
    }

    /// Enumerate ADB USB devices as backend entries. Devices without a USB
    /// serial are skipped: the host protocol identifies devices by serial, and
    /// a serial-less device cannot be addressed or transport-id'd.
    fn enumerate_usb() -> Vec<DeviceEntry> {
        match find_all_connected_adb_devices() {
            Ok(devices) => devices
                .into_iter()
                .filter_map(|d| {
                    d.serial
                        .map(|s| DeviceEntry::new(s).with_kind(TransportKind::Usb))
                })
                .collect(),
            Err(e) => {
                tracing::warn!("DefaultDeviceBackend: device enumeration failed: {e}");
                Vec::new()
            }
        }
    }

    /// The unified device set: USB (hotplug-enumerated) + TCP (`host:connect`ed).
    /// This is the single source of truth so `host:devices`, `devices-l`,
    /// `track-devices`, and transport-id all see the same merged list — mirroring
    /// AOSP's single `transport_list`.
    async fn enumerate_all(&self) -> Vec<DeviceEntry> {
        let tcp_entries = tcp_device_entries(&self.tcp_devices).await;
        merge_device_sets(Self::enumerate_usb(), tcp_entries)
    }

    /// Normalize an `adb connect`/`disconnect` target into a `<ip>:<port>`
    /// `SocketAddr` (+ its canonical string serial). A missing port defaults to
    /// [`DEFAULT_ADB_TCP_PORT`]. Resolves hostnames via the system resolver.
    fn resolve_tcp_target(addr: &str) -> Result<(SocketAddr, String)> {
        // `host:port` (literal or hostname) resolves directly; a bare `host` /
        // IP with no port gets the default adbd-over-TCP port appended first.
        // `to_socket_addrs` needs a port, so the no-port case must add one.
        let resolved = addr
            .to_socket_addrs()
            .ok()
            .and_then(|mut it| it.next())
            .or_else(|| {
                format!("{addr}:{DEFAULT_ADB_TCP_PORT}")
                    .to_socket_addrs()
                    .ok()
                    .and_then(|mut it| it.next())
            });
        let socket = resolved.ok_or_else(|| {
            RustADBError::ADBRequestFailed(format!("could not resolve address '{addr}'"))
        })?;
        Ok((socket, socket.to_string()))
    }

    /// Gracefully close every cached device connection.
    ///
    /// Call this on server teardown (SIGTERM / shutdown), BEFORE the process /
    /// runtime starts tearing tasks down. Each connection flushes a single
    /// connection-level CLSE while its writer task is still alive, so the device
    /// tears every multiplexed stream down cleanly. Without this, connections are
    /// only dropped at process exit — when the writer task may already be gone, so
    /// the fire-and-forget CLSE fails and the device is left with orphaned streams
    /// that reject the next CNXN with a stale CLSE (the selftest flaky-SKIP cause).
    ///
    /// Drains the cache so a re-used backend re-opens fresh connections. Idempotent.
    pub async fn shutdown(&self) {
        let conns: Vec<Arc<PersistentUsbConnection>> = {
            let mut map = self.conns.lock().await;
            map.drain().map(|(_serial, conn)| conn).collect()
        };
        for conn in conns {
            // `shutdown` takes `&self` and is idempotent; any extra `Arc` clones
            // held by reverse pumps will observe the connection already closed.
            conn.shutdown().await;
        }
        // Reverse pumps key off the (now-closed) connections' readers stopping;
        // clear the map so a restarted backend rebuilds them.
        self.reverse.lock().await.clear();
        // Gracefully close every tracked TCP device's persistent connection
        // (flush one connection-level CLSE while the writer is alive), then drop
        // them — same teardown discipline as the USB pool above.
        let tcp_conns: Vec<Arc<PersistentTcpConnection>> = {
            let mut map = self.tcp_devices.lock().await;
            map.drain().map(|(_serial, conn)| conn).collect()
        };
        for conn in tcp_conns {
            conn.shutdown().await;
        }
    }

    /// The live persistent connection for a `host:connect`ed TCP serial, if one
    /// is registered (and its reader task is still alive). `None` for a USB
    /// serial or a dropped/dead TCP connection, so callers fall through to the
    /// USB `get_or_open` path.
    async fn tcp_conn(&self, serial: &str) -> Option<Arc<PersistentTcpConnection>> {
        self.tcp_devices
            .lock()
            .await
            .get(serial)
            .filter(|conn| conn.is_alive())
            .map(Arc::clone)
    }

    /// Get the cached connection for `serial`, opening (and caching) it on first
    /// use. A dead cached connection (reader task exited) is replaced.
    ///
    /// Right after a USB re-enumeration (adbd restart from
    /// `root:`/`unroot:`/`tcpip:`/`usb:`/reboot, or a physical replug) the device
    /// can be briefly absent from enumeration, or enumerated-but-not-ready so the
    /// handshake's first transfer races the not-ready endpoint. This is exactly
    /// the `adb root` reconnect path: the client waits for the device to return
    /// then immediately issues the next service, and a bare `new_from_serial` with
    /// zero retry would fail. So the open is wrapped in a bounded
    /// [`OPEN_RETRY_BUDGET`] retry over transient transfer errors + brief
    /// `DeviceNotFound`, mirroring AOSP's bounded transport-reconnect handler
    /// (and the consumer-side `open_device_with_retry` the selftest proved out).
    ///
    /// CRITICAL: the `conns` mutex is **released** across the multi-second retry
    /// (it only guards the cache lookup/insert, not the open). Holding it across
    /// the retry would serialize every other `get_or_open` caller behind one
    /// device's re-enumeration window.
    async fn get_or_open(&self, serial: &str) -> Result<Arc<PersistentUsbConnection>> {
        // Fast path: a live cached connection. Reap a dead one while holding the
        // lock (cheap, no I/O), then drop the lock before any open attempt.
        {
            let mut conns = self.conns.lock().await;
            if let Some(conn) = conns.get(serial) {
                if conn.is_alive() {
                    return Ok(Arc::clone(conn));
                }
                // Stale: the device was unplugged or the connection died. Drop it
                // and re-open below (outside the lock).
                conns.remove(serial);
            }
        }

        // Open OUTSIDE the lock, bounded so a truly-absent device fails fast.
        let key_path = self.private_key_path.clone();
        let conn = retry_within(
            OPEN_RETRY_BUDGET,
            OPEN_RETRY_POLL,
            is_retryable_open_error,
            || PersistentUsbConnection::new_from_serial(serial, key_path.clone()),
        )
        .await
        .map(Arc::new)?;

        // Re-acquire the lock to publish. Another task may have opened the same
        // serial while we were unlocked; if a live connection is already cached,
        // prefer it (drop ours) so all callers share one connection per device.
        let mut conns = self.conns.lock().await;
        if let Some(existing) = conns.get(serial)
            && existing.is_alive()
        {
            return Ok(Arc::clone(existing));
        }
        conns.insert(serial.to_owned(), Arc::clone(&conn));
        // Spawn the connection-death → TransportReset watcher ONLY if the
        // lifecycle hub already exists (a frontend has subscribed). Before that,
        // nobody consumes the event, so there is nothing to publish to. The
        // frontend subscribes at `serve()` time, well before any service request
        // reaches `get_or_open`, so in practice the hub is always present here.
        if let Some(tx) = self.lifecycle.get() {
            Self::spawn_transport_reset_watch(tx.clone(), serial.to_owned(), &conn);
        }
        Ok(conn)
    }

    /// Open a session on `serial`, tolerating a connection that dies on its very
    /// first OPEN right after re-enumeration (the second of the two
    /// post-re-enumeration races).
    ///
    /// `get_or_open` rides out the CNXN race, but the first OPEN frame is sent by
    /// the connection's writer task and can itself hit a transient on a freshly
    /// re-enumerated device. The writer's fatal arm then tears the connection
    /// down (deliberately, for OUT-stream truncation safety — see `persistent.rs`),
    /// so `open_session` fails and `is_alive()` flips false. Rather than mutate
    /// the shared writer loop, we treat a dead-on-first-OPEN connection like a
    /// failed connect: drop the (now-dead) cached connection and reopen+retry
    /// within the same [`OPEN_RETRY_BUDGET`]. A failure on a still-alive connection
    /// is a real service rejection (e.g. CLSE) and is returned immediately.
    async fn open_session_with_reopen(
        &self,
        serial: &str,
        cmd: &ADBLocalCommand,
    ) -> Result<MultiplexedSession> {
        let deadline = Instant::now() + OPEN_RETRY_BUDGET;
        loop {
            let conn = self.get_or_open(serial).await?;
            match conn.open_session(cmd).await {
                Ok(session) => return Ok(session),
                // Only retry when the FAILURE killed the connection (first-OPEN
                // race). A failure on a live connection is a genuine rejection.
                Err(e) if conn.is_alive() || Instant::now() >= deadline => return Err(e),
                Err(e) => {
                    tracing::debug!(
                        serial = %serial,
                        "open_session died on first OPEN ({e}); dropping dead connection and reopening"
                    );
                    self.conns.lock().await.remove(serial);
                    tokio::time::sleep(OPEN_RETRY_POLL).await;
                }
            }
        }
    }

    /// The lifecycle broadcast sender, initializing the hub (and spawning the
    /// one-shot USB hotplug-diff watcher that feeds it) on first use.
    ///
    /// Idempotent: the `OnceLock` makes hub creation race-free, and the
    /// `AtomicBool` ensures the watcher task is spawned exactly once even if two
    /// `subscribe_lifecycle` calls land together.
    fn lifecycle_tx(&self) -> broadcast::Sender<LifecycleEvent> {
        let tx = self
            .lifecycle
            .get_or_init(|| broadcast::channel(LIFECYCLE_CHANNEL_SIZE).0)
            .clone();
        if !self.lifecycle_watch_started.swap(true, Ordering::SeqCst) {
            Self::spawn_usb_disconnect_watch(tx.clone());
        }
        tx
    }

    /// Emit a [`LifecycleEvent::Disconnected`] for `serial` if the lifecycle hub
    /// has been initialized. A no-op before the first `subscribe_lifecycle`
    /// (nobody is listening yet) and when all receivers have lagged/closed —
    /// disconnect cleanup is best-effort, never fatal.
    fn emit_disconnected(&self, serial: &str) {
        if let Some(tx) = self.lifecycle.get() {
            // `send` errs only when there are no live receivers; that's fine.
            let _ = tx.send(LifecycleEvent::Disconnected(serial.to_owned()));
        }
    }

    /// Spawn a one-shot watcher that awaits `conn`'s death and then publishes a
    /// [`LifecycleEvent::TransportReset`] for `serial`.
    ///
    /// This is the event-driven half of the disconnect detection (the entry
    /// `transport_alive` check is the primary path; PR0 data showed the
    /// connection usually dies *before* the `wait-for-disconnect` request even
    /// arrives, but this covers the minority case where the wait arrives first).
    /// The watcher awaits a standalone death future
    /// ([`PersistentUsbConnection::closed_signal`]) that holds NO reference to the
    /// connection, so it never pins a dead connection alive: replacing/reaping
    /// the cache entry drops the `Arc` and lets its reader/writer be aborted,
    /// while the death future still resolves (the tasks fire the signal on exit
    /// regardless of who holds the connection). One watcher per cached
    /// connection; the edge is never lost even if death already happened.
    fn spawn_transport_reset_watch(
        tx: broadcast::Sender<LifecycleEvent>,
        serial: String,
        conn: &Arc<PersistentUsbConnection>,
    ) {
        let death = conn.closed_signal();
        tokio::spawn(async move {
            death.await;
            tracing::debug!(
                serial = %serial,
                "cached connection died; emitting TransportReset lifecycle event"
            );
            // `send` errs only when no live receivers remain; best-effort,
            // never fatal (mirrors `emit_disconnected`).
            let _ = tx.send(LifecycleEvent::TransportReset(serial));
        });
    }

    /// Spawn the single USB hotplug-diff watcher. It maintains the set of
    /// present ADB USB serials and, on each hotplug event, re-enumerates and
    /// emits [`LifecycleEvent::Disconnected`] for every serial that left.
    ///
    /// Separate from `subscribe_changes`'s watcher: that one pushes full
    /// snapshots to one `track-devices` client and carries no
    /// disappeared-since-last-time memory. Lifecycle cleanup needs the *diff*
    /// (which serial vanished), so it keeps its own previous-set state.
    fn spawn_usb_disconnect_watch(tx: broadcast::Sender<LifecycleEvent>) {
        let watch = match nusb::watch_devices() {
            Ok(w) => w,
            Err(e) => {
                // No hotplug on this platform/setup: USB disconnects won't be
                // auto-detected. TCP disconnects still emit (synchronous path),
                // and stale USB connections are still reaped lazily on next use.
                tracing::warn!(
                    "DefaultDeviceBackend: hotplug unavailable ({e}); USB disconnect auto-release disabled"
                );
                return;
            }
        };
        tokio::spawn(async move {
            let mut present: HashSet<String> = Self::enumerate_usb_serials();
            let mut watch = watch;
            while watch.next().await.is_some() {
                let now = Self::enumerate_usb_serials();
                // Serials in the previous set but not the current one disconnected.
                for gone in present.difference(&now) {
                    tracing::info!(serial = %gone, "USB device disconnected; emitting lifecycle event");
                    if tx.send(LifecycleEvent::Disconnected(gone.clone())).is_err() {
                        // All receivers dropped → frontend gone; stop watching.
                        return;
                    }
                }
                present = now;
            }
            tracing::debug!("DefaultDeviceBackend: lifecycle hotplug watch ended");
        });
    }

    /// The set of currently-present ADB USB device serials (serial-less devices
    /// are skipped, matching [`Self::enumerate_usb`]).
    fn enumerate_usb_serials() -> HashSet<String> {
        match find_all_connected_adb_devices() {
            Ok(devices) => devices.into_iter().filter_map(|d| d.serial).collect(),
            Err(e) => {
                tracing::warn!("DefaultDeviceBackend: serial enumeration failed: {e}");
                HashSet::new()
            }
        }
    }
}

impl DeviceBackend for DefaultDeviceBackend {
    async fn list_devices(&self) -> Vec<DeviceEntry> {
        self.enumerate_all().await
    }

    async fn subscribe_lifecycle(&self) -> mpsc::Receiver<LifecycleEvent> {
        // Initialize the hub + spawn the one-shot USB hotplug-diff watcher, then
        // bridge this subscriber's broadcast receiver onto an mpsc the frontend
        // consumes (the trait surface is mpsc to match `subscribe_changes`). The
        // bridge task ends when either the broadcast closes or the frontend drops
        // its mpsc receiver. Lagged events (slow consumer) are logged and skipped
        // rather than aborting the stream.
        let mut bcast = self.lifecycle_tx().subscribe();
        let (tx, rx) = mpsc::channel(LIFECYCLE_CHANNEL_SIZE);
        tokio::spawn(async move {
            loop {
                match bcast.recv().await {
                    Ok(ev) => {
                        if tx.send(ev).await.is_err() {
                            break; // frontend dropped its receiver
                        }
                    }
                    Err(broadcast::error::RecvError::Lagged(n)) => {
                        tracing::warn!("subscribe_lifecycle: lagged, dropped {n} event(s)");
                    }
                    Err(broadcast::error::RecvError::Closed) => break,
                }
            }
        });
        rx
    }

    async fn transport_alive(&self, serial: &str) -> bool {
        // Primary path for `wait-for-disconnect`: a cached connection whose
        // reader/writer died reads as NOT alive even while the device is still
        // enumerated (the bug fix — an adbd restart that does not re-enumerate USB
        // never leaves `list_devices`). Only when there is no cached connection do
        // we fall back to presence — that's the genuine "never opened / already
        // reaped" case, where enumeration is the best available signal.
        if let Some(conn) = self.conns.lock().await.get(serial) {
            return conn.is_alive();
        }
        if let Some(conn) = self.tcp_devices.lock().await.get(serial) {
            return conn.is_alive();
        }
        self.list_devices().await.iter().any(|d| d.serial == serial)
    }

    async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
        let (tx, rx) = mpsc::channel(CHANGES_CHANNEL_SIZE);

        // Each snapshot merges live USB enumeration with the current TCP-device
        // registry, so `track-devices` reflects `host:connect`ed devices too.
        // The watcher holds an `Arc` clone of the TCP map for this reason.
        let tcp_devices = Arc::clone(&self.tcp_devices);
        let snapshot = move || {
            let tcp = Arc::clone(&tcp_devices);
            async move {
                let tcp_entries = tcp_device_entries(&tcp).await;
                merge_device_sets(Self::enumerate_usb(), tcp_entries)
            }
        };

        // Spawn a watcher that pushes a full snapshot on every hotplug event.
        // We start the watch BEFORE the initial enumeration so a device added
        // in the gap is not missed (nusb's documented ordering guidance), then
        // send the initial snapshot. If hotplug is unavailable on this platform
        // / setup, fall back to sending one snapshot and closing.
        match nusb::watch_devices() {
            Ok(watch) => {
                tokio::spawn(async move {
                    // Initial snapshot.
                    if tx.send(snapshot().await).await.is_err() {
                        return; // receiver already gone
                    }
                    let mut watch = watch;
                    // Each hotplug event (connect/disconnect) → re-enumerate and
                    // push a fresh full snapshot. Coalescing is unnecessary: the
                    // consumer only cares about the latest set.
                    while watch.next().await.is_some() {
                        if tx.send(snapshot().await).await.is_err() {
                            break; // receiver dropped → stop watching
                        }
                    }
                    tracing::debug!("DefaultDeviceBackend: hotplug watch ended");
                });
            }
            Err(e) => {
                tracing::warn!(
                    "DefaultDeviceBackend: hotplug unavailable ({e}); sending a single device snapshot"
                );
                tokio::spawn(async move {
                    let _ = tx.send(snapshot().await).await;
                });
            }
        }

        rx
    }

    async fn open_local_service(
        &self,
        serial: &str,
        cmd: &ADBLocalCommand,
    ) -> Result<MultiplexedSession> {
        // Reject services the bridge does not support BEFORE opening a session,
        // with a stable reason (mirrors the frontend's pre-open guard). Bridged:
        // `shell:` v1 (empty-args ShellCommand), `tcp:`, and the verbatim `Raw`
        // pass-through (the frontend uses `Raw` for `sync:` / `shell,v2`, which
        // it has already capability-gated).
        match cmd {
            ADBLocalCommand::ShellCommand(_, args) if args.is_empty() => {}
            ADBLocalCommand::TcpConnect(_) | ADBLocalCommand::Raw(_) => {}
            other => {
                return Err(RustADBError::ADBRequestFailed(format!(
                    "DefaultDeviceBackend: unsupported local service: {other}"
                )));
            }
        }
        // A `host:connect`ed TCP serial is served by its own persistent
        // multiplexed connection (same transport-free session type as USB);
        // anything else is a USB serial opened on demand.
        if let Some(conn) = self.tcp_conn(serial).await {
            return conn.open_session(cmd).await;
        }
        // The `adb root` reconnect path: the device just re-enumerated, so cover
        // BOTH races — the CNXN race (inside `get_or_open`'s open retry) and the
        // first-OPEN race (drop the dead connection + reopen).
        self.open_session_with_reopen(serial, cmd).await
    }

    async fn open_sync_session(&self, serial: &str) -> Result<SyncSession> {
        if let Some(conn) = self.tcp_conn(serial).await {
            return conn.open_sync_session().await;
        }
        let conn = self.get_or_open(serial).await?;
        conn.open_sync_session().await
    }

    async fn open_shell_v2(&self, serial: &str, cmd: &str) -> Result<ShellV2Session> {
        if let Some(conn) = self.tcp_conn(serial).await {
            return conn.open_shell_v2(cmd).await;
        }
        let conn = self.get_or_open(serial).await?;
        conn.open_shell_v2(cmd).await
    }

    async fn capabilities(&self) -> BackendCapabilities {
        BackendCapabilities {
            sync: true,
            shell_v2: true,
            reverse: true,
        }
    }

    async fn device_capabilities(
        &self,
        serial: &str,
        timeout: std::time::Duration,
    ) -> Option<DeviceFeatureSet> {
        // Cache-first: a live TCP or USB connection already parsed this device's
        // banner at handshake time, so return its peer_features() with no I/O.
        if let Some(conn) = self.tcp_conn(serial).await {
            return Some(conn.peer_features().clone());
        }
        if let Some(conn) = self.conns.lock().await.get(serial)
            && conn.is_alive()
        {
            return Some(conn.peer_features().clone());
        }
        // Not yet connected: establish the USB connection (which performs the CNXN
        // handshake and parses the banner), bounded by `timeout`. `get_or_open`
        // caches it, so the cost is paid once and subsequent queries hit the cache
        // above. On timeout or any open error, report `None` (unknown) so the
        // frontend stays conservative rather than guessing capabilities.
        match tokio::time::timeout(timeout, self.get_or_open(serial)).await {
            Ok(Ok(conn)) => Some(conn.peer_features().clone()),
            Ok(Err(e)) => {
                tracing::debug!(
                    "device_capabilities({serial}): open failed: {e}; reporting unknown"
                );
                None
            }
            Err(_elapsed) => {
                tracing::debug!(
                    "device_capabilities({serial}): handshake exceeded {timeout:?}; reporting unknown"
                );
                None
            }
        }
    }

    async fn open_reverse(&self, serial: &str, remote: &str, local: &str) -> Result<()> {
        self.reverse_engine(serial).await?.open(remote, local).await
    }

    async fn reverse_remove(&self, serial: &str, remote: &str) -> Result<()> {
        self.reverse_engine(serial).await?.remove(remote).await
    }

    async fn reverse_remove_all(&self, serial: &str) -> Result<()> {
        self.reverse_engine(serial).await?.remove_all().await
    }

    async fn release_reverse(&self, serial: &str) -> Result<()> {
        // Disconnect path: do NOT route through `reverse_engine`, which would
        // re-open the (just-unplugged) connection to reach the engine. The
        // device is gone, so its inbound-open pump is already stopping; we only
        // need to drop the in-memory rule entry. Removing the engine from the
        // map drops the last non-pump `Arc`, so its rules stop showing in
        // `list_reverse` and the entry no longer leaks until `shutdown()`.
        if self.reverse.lock().await.remove(serial).is_some() {
            tracing::debug!(
                serial,
                "release_reverse: dropped reverse engine on disconnect"
            );
        }
        Ok(())
    }

    async fn list_reverse(&self, serial: &str) -> Result<String> {
        // The host's own rule registry is the source of truth for what this
        // server set up; render it directly (the device's list-forward would also
        // include other clients' rules). No engine yet → no rules.
        match self.reverse.lock().await.get(serial) {
            Some(engine) => Ok(engine.list().await),
            None => Ok(String::new()),
        }
    }

    async fn connect(&self, addr: &str) -> Result<String> {
        let (socket, serial) = Self::resolve_tcp_target(addr)?;

        // Idempotent like AOSP: a re-connect to an already-tracked device is not
        // an error.
        if self.tcp_devices.lock().await.contains_key(&serial) {
            return Ok(format!("already connected to {serial}"));
        }

        // Perform the full CNXN(+AUTH, +STLS) handshake now so a failure is
        // reported to the client synchronously (AOSP `adb connect` blocks on the
        // connect), and so the device only joins the list once it is actually
        // reachable. A persistent multiplexed connection (not a one-shot
        // `ADBTcpDevice`) so local services can later be bridged through it.
        let conn =
            PersistentTcpConnection::new_from_tcp_addr(socket, self.private_key_path.clone())
                .await
                .map_err(|e| {
                    RustADBError::ADBRequestFailed(format!("failed to connect to {serial}: {e}"))
                })?;

        // Re-check under the lock to avoid a TOCTOU double-insert when two
        // `connect`s for the same addr race; keep the first, drop our extra.
        let mut map = self.tcp_devices.lock().await;
        if map.contains_key(&serial) {
            return Ok(format!("already connected to {serial}"));
        }
        map.insert(serial.clone(), Arc::new(conn));
        tracing::info!("host:connect registered TCP/IP device {serial}");
        Ok(format!("connected to {serial}"))
    }

    async fn disconnect(&self, addr: &str) -> Result<String> {
        // Empty target → disconnect every TCP device (AOSP `adb disconnect`).
        if addr.is_empty() {
            let gone: Vec<String> = {
                let mut map = self.tcp_devices.lock().await;
                map.drain().map(|(serial, _)| serial).collect()
            };
            let n = gone.len();
            // Each removed serial's forward/reverse rules are now stale: emit a
            // lifecycle event so the frontend releases them per its policy.
            for serial in &gone {
                self.emit_disconnected(serial);
            }
            return Ok(format!("disconnected everything ({n} device(s))"));
        }

        let (_socket, serial) = Self::resolve_tcp_target(addr)?;
        if self.tcp_devices.lock().await.remove(&serial).is_some() {
            tracing::info!("host:disconnect removed TCP/IP device {serial}");
            self.emit_disconnected(&serial);
            Ok(format!("disconnected {serial}"))
        } else {
            Err(RustADBError::ADBRequestFailed(format!(
                "no such device {serial}"
            )))
        }
    }
}

/// Merge the USB device entries with the pre-built `host:connect`ed TCP entries
/// into the single unified device set. Pure (sans-io) so the merge shape is
/// unit-tested without hardware.
fn merge_device_sets(
    usb: Vec<DeviceEntry>,
    tcp_entries: impl IntoIterator<Item = DeviceEntry>,
) -> Vec<DeviceEntry> {
    let mut all = usb;
    all.extend(tcp_entries);
    all
}

/// Build one [`DeviceEntry`] per `host:connect`ed TCP device, carrying the
/// device's parsed banner capabilities ([`PersistentConnection::peer_features`]).
///
/// A tracked TCP device is, by construction, connected — its CNXN handshake
/// already happened at `host:connect` time — so its capabilities are known
/// (`Some`) here with no extra I/O. TCP devices use the default
/// `DeviceState::Device`.
async fn tcp_device_entries(
    tcp_devices: &Mutex<HashMap<String, Arc<PersistentTcpConnection>>>,
) -> Vec<DeviceEntry> {
    tcp_devices
        .lock()
        .await
        .iter()
        .map(|(serial, conn)| {
            DeviceEntry::new(serial.clone())
                .with_kind(TransportKind::Local)
                .with_capabilities(conn.peer_features().clone())
        })
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn resolve_tcp_target_appends_default_port_when_missing() {
        // `adb connect 127.0.0.1` → adbd-over-TCP default port 5555.
        let (socket, serial) = DefaultDeviceBackend::resolve_tcp_target("127.0.0.1").unwrap();
        assert_eq!(socket.port(), DEFAULT_ADB_TCP_PORT);
        assert_eq!(serial, "127.0.0.1:5555");
    }

    #[test]
    fn resolve_tcp_target_keeps_explicit_port() {
        let (socket, serial) = DefaultDeviceBackend::resolve_tcp_target("127.0.0.1:8885").unwrap();
        assert_eq!(socket.port(), 8885);
        assert_eq!(serial, "127.0.0.1:8885");
    }

    #[test]
    fn resolve_tcp_target_rejects_garbage() {
        // A non-resolvable host must surface an error, not silently default.
        assert!(DefaultDeviceBackend::resolve_tcp_target("not a host:::").is_err());
    }

    #[test]
    fn merge_device_sets_appends_tcp_serials_to_usb() {
        // The unified device set is USB entries + one entry per TCP serial. This
        // is what `host:devices`/`devices-l`/transport-id are computed over, so it
        // must include both kinds with no dropped entry. (Tested on the pure
        // merge helper since a real ADBTcpDevice needs hardware.)
        let usb = vec![DeviceEntry::new("USBSERIAL01")];
        let tcp_entries = [
            DeviceEntry::new("10.0.0.5:5555"),
            DeviceEntry::new("10.0.0.6:5555"),
        ];
        let merged = merge_device_sets(usb, tcp_entries);
        let serials: Vec<&str> = merged.iter().map(|d| d.serial.as_str()).collect();
        assert!(serials.contains(&"USBSERIAL01"), "USB kept: {serials:?}");
        assert!(
            serials.contains(&"10.0.0.5:5555"),
            "TCP #1 added: {serials:?}"
        );
        assert!(
            serials.contains(&"10.0.0.6:5555"),
            "TCP #2 added: {serials:?}"
        );
        assert_eq!(merged.len(), 3, "no entry dropped or duplicated");
    }

    #[tokio::test]
    async fn tcp_conn_is_none_for_unregistered_serial() {
        // The routing decision in `open_local_service`/`open_sync_session`/
        // `open_shell_v2`: a serial not in the TCP registry yields `None`, so the
        // caller falls through to the USB `get_or_open` path. (A real registered
        // TCP connection needs a live device, so we assert the decision, not the
        // bridge — mirroring how `merge_device_sets` is unit-tested.)
        let backend = DefaultDeviceBackend::new();
        assert!(
            backend.tcp_conn("USBSERIAL01").await.is_none(),
            "USB-style serial must not resolve to a TCP connection"
        );
        assert!(
            backend.tcp_conn("10.0.0.5:5555").await.is_none(),
            "an un-connected TCP serial must not resolve to a TCP connection"
        );
    }

    #[tokio::test]
    async fn open_local_service_no_longer_reports_tcp_not_yet_supported() {
        // PR4b removed the stable "not yet supported" guard for TCP serials. With
        // no device of that serial present at all, the call still fails (no
        // hardware), but it must NOT be the old TCP-unsupported message — it must
        // route to the USB path instead.
        let backend = DefaultDeviceBackend::new();
        let cmd = ADBLocalCommand::ShellCommand(String::new(), vec![]);
        // `MultiplexedSession` is not `Debug`, so inspect the error directly
        // rather than via `unwrap_err`.
        let Err(err) = backend.open_local_service("10.0.0.5:5555", &cmd).await else {
            panic!("expected an error without a real device");
        };
        assert!(
            !format!("{err}").contains("not yet supported"),
            "TCP local-service bridging is wired now; got: {err}"
        );
    }

    #[tokio::test]
    async fn disconnect_empty_addr_reports_count_and_clears() {
        let backend = DefaultDeviceBackend::new();
        // Empty target is the "disconnect everything" path; with nothing
        // registered it succeeds and reports zero.
        let msg = backend.disconnect("").await.unwrap();
        assert!(msg.contains("disconnected everything"), "got: {msg}");
        assert!(msg.contains("0 device"), "got: {msg}");
    }

    #[tokio::test]
    async fn disconnect_unknown_addr_fails() {
        let backend = DefaultDeviceBackend::new();
        let err = backend.disconnect("127.0.0.1:5555").await.unwrap_err();
        assert!(format!("{err}").contains("no such device"), "got: {err}");
    }

    #[tokio::test]
    async fn device_capabilities_reports_none_for_unconnectable_serial() {
        // No device of this serial is connected, and a fresh USB open will fail
        // (no hardware) — possibly slowly — so the bounded query must report
        // `None` (unknown) rather than block or guess. A tiny timeout keeps the
        // test fast and also exercises the timeout arm.
        let backend = DefaultDeviceBackend::new();
        let caps = backend
            .device_capabilities("USBSERIAL01", std::time::Duration::from_millis(50))
            .await;
        assert!(
            caps.is_none(),
            "an unconnectable device must report unknown (None) capabilities"
        );
    }

    // ---- retry_within policy (Module B) --------------------------------------

    #[tokio::test]
    async fn retry_within_succeeds_after_n_transient_failures() {
        // Fails twice (retryable), then succeeds — within budget.
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let result: Result<u32> = retry_within(
            Duration::from_secs(5),
            Duration::from_millis(1),
            |_| true, // everything retryable for this test
            || {
                let attempts = std::sync::Arc::clone(&attempts);
                async move {
                    let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n < 2 {
                        Err(RustADBError::DeviceNotFound("absent".into()))
                    } else {
                        Ok(42)
                    }
                }
            },
        )
        .await;
        assert_eq!(
            result.ok(),
            Some(42),
            "retry_within must succeed once the op stops failing within budget"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            3,
            "must have retried exactly until the third (successful) attempt"
        );
    }

    #[tokio::test]
    async fn retry_within_gives_up_within_budget_when_always_failing() {
        // Always fails (retryable) → must give up at the budget, not hang.
        let start = Instant::now();
        let result: Result<u32> = retry_within(
            Duration::from_millis(50),
            Duration::from_millis(5),
            |_| true,
            || async { Err(RustADBError::DeviceNotFound("gone".into())) },
        )
        .await;
        assert!(
            result.is_err(),
            "a permanently-failing op must surface the last error after the budget"
        );
        assert!(
            start.elapsed() < Duration::from_secs(2),
            "must fail fast within ~budget, not hang (elapsed {:?})",
            start.elapsed()
        );
    }

    #[tokio::test]
    async fn retry_within_returns_immediately_on_non_retryable_error() {
        // A non-retryable error must NOT be retried, even with budget remaining.
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let result: Result<u32> = retry_within(
            Duration::from_secs(60),
            Duration::from_millis(1),
            |_| false, // nothing retryable
            || {
                let attempts = std::sync::Arc::clone(&attempts);
                async move {
                    attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    Err(RustADBError::DeviceBusy)
                }
            },
        )
        .await;
        assert!(result.is_err(), "a non-retryable error must propagate");
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            1,
            "a non-retryable error must be returned on the first attempt (no retry)"
        );
    }

    #[test]
    fn is_retryable_open_error_is_the_reopen_window_family() {
        use nusb::transfer::TransferError;
        // RETRYABLE — the reopen-window family. The outer layer rebuilds the
        // transport each poll (the only thing that recovers re-enumeration); the
        // 10 s wall clock keeps a real fault/unplug failing fast.
        assert!(
            is_retryable_open_error(&RustADBError::DeviceNotFound("absent".into())),
            "DeviceNotFound (device momentarily not enumerated) must be retryable"
        );
        assert!(
            is_retryable_open_error(&RustADBError::UsbTransferError(TransferError::Disconnected)),
            "Disconnected (NoDevice, re-enumeration gap) must be retryable"
        );
        assert!(
            is_retryable_open_error(&RustADBError::UsbTransferError(TransferError::Stall)),
            "Stall (re-enumerated endpoint stalls until a fresh connect) must be retryable AT THE REOPEN LAYER — this REVERSES the v1 decoupling"
        );
        assert!(
            is_retryable_open_error(&RustADBError::UsbTransferError(TransferError::Unknown(
                0xe000_02ed
            ))),
            "NotResponding (0xe00002ed) lands in Unknown(_) and must be retryable"
        );
        assert!(
            is_retryable_open_error(&RustADBError::UsbTransferError(TransferError::Unknown(
                0xe000_02d8
            ))),
            "NotReady (0xe00002d8) — the NEW code — is covered by the Unknown(_) family, no new constant needed (ends the whack-a-mole)"
        );
        assert!(
            is_retryable_open_error(&RustADBError::ADBRequestFailed(
                "CNXN failed after 8 attempts (stale CLSE or transient transfer error)".into()
            )),
            "a CNXN-exhausted ADBRequestFailed is the explicit 'old transport dead, reopen' signal and MUST be retried — this REVERSES the v1 anti-amplification decoupling (now safe: inner is a small constant, total ≈ outer wall clock, not a product)"
        );

        // FATAL — must fail fast, never masked by the budget.
        assert!(
            !is_retryable_open_error(&RustADBError::UsbTransferError(
                TransferError::InvalidArgument
            )),
            "InvalidArgument (programming error / unsupported request) must NOT be retried"
        );
        assert!(
            !is_retryable_open_error(&RustADBError::UsbTransferError(TransferError::Fault)),
            "Fault (hardware issue / protocol violation) must NOT be retried"
        );
        assert!(
            !is_retryable_open_error(&RustADBError::DeviceBusy),
            "DeviceBusy (another process holds the single USB claim) must NOT be retried"
        );
    }

    #[tokio::test]
    async fn retry_within_redrives_cnxn_exhaustion_by_reopening() {
        // The CORE outer-layer fix: a first attempt whose handshake exhausts (the
        // CNXN-exhausted `ADBRequestFailed` out of `do_connect` on a now-dead
        // transport) must be RE-DRIVEN by `retry_within` — the next poll rebuilds a
        // fresh transport (modelled by the closure returning Ok the second time),
        // which the back-to-back `adb root; adb unroot` trace proved is the only
        // thing that recovers re-enumeration. This locks the v1 reversal: the outer
        // layer now owns re-enumeration recovery through `is_retryable_open_error`.
        let attempts = std::sync::Arc::new(std::sync::atomic::AtomicU32::new(0));
        let result: Result<u32> = retry_within(
            Duration::from_secs(5),
            Duration::from_millis(1),
            is_retryable_open_error,
            || {
                let attempts = std::sync::Arc::clone(&attempts);
                async move {
                    let n = attempts.fetch_add(1, std::sync::atomic::Ordering::SeqCst);
                    if n == 0 {
                        // First (stale) transport: CNXN exhausts on the dead handle.
                        Err(RustADBError::ADBRequestFailed(
                            "CNXN failed after 8 attempts (stale CLSE or transient transfer error)"
                                .into(),
                        ))
                    } else {
                        // Reopen rebuilt a fresh transport → handshake succeeds.
                        Ok(7)
                    }
                }
            },
        )
        .await;
        assert_eq!(
            result.ok(),
            Some(7),
            "retry_within must re-drive a CNXN-exhausted ADBRequestFailed by reopening (fresh transport) and then succeed"
        );
        assert_eq!(
            attempts.load(std::sync::atomic::Ordering::SeqCst),
            2,
            "exactly one reopen: first attempt exhausts CNXN, second (fresh transport) succeeds"
        );
    }
}
