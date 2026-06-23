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
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::{Arc, OnceLock};

use futures_util::StreamExt;
use tokio::sync::{Mutex, broadcast, mpsc};

use super::backend::{BackendCapabilities, DeviceBackend, DeviceEntry, LifecycleEvent};
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
                .filter_map(|d| d.serial.map(DeviceEntry::new))
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
    async fn get_or_open(&self, serial: &str) -> Result<Arc<PersistentUsbConnection>> {
        let mut conns = self.conns.lock().await;
        if let Some(conn) = conns.get(serial) {
            if conn.is_alive() {
                return Ok(Arc::clone(conn));
            }
            // Stale: the device was unplugged or the connection died. Drop it
            // and re-open below.
            conns.remove(serial);
        }
        let conn = PersistentUsbConnection::new_from_serial(serial, self.private_key_path.clone())
            .await
            .map(Arc::new)?;
        conns.insert(serial.to_owned(), Arc::clone(&conn));
        Ok(conn)
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
        let conn = self.get_or_open(serial).await?;
        conn.open_session(cmd).await
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
            DeviceEntry::new(serial.clone()).with_capabilities(conn.peer_features().clone())
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
}
