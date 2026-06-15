//! The bundled, zero-config default device backend (USB + TCP/IP).
//!
//! [`DefaultDeviceBackend`] is the default [`DeviceBackend`]: it enumerates ADB
//! USB devices, tracks `host:connect`ed TCP/IP devices, opens local services
//! over a [`PersistentConnection`], and reports device-set changes via nusb
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

use std::collections::HashMap;
use std::net::{SocketAddr, ToSocketAddrs};
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};

use super::backend::{BackendCapabilities, DeviceBackend, DeviceEntry};
use crate::models::ADBLocalCommand;
use crate::tcp::tcp_transport::TcpTransport;
use crate::usb::persistent::PersistentConnection;
use crate::usb::{
    MultiplexedSession, PersistentUsbConnection, ReverseEngine, ReversePolicy, ShellV2Session,
    SyncSession, find_all_connected_adb_devices,
};
use crate::{Result, RustADBError};

/// A persistent multiplexed connection to a `host:connect`ed TCP/IP device —
/// the TCP analogue of [`PersistentUsbConnection`]. Local services (`shell:` /
/// `tcp:` / `sync:` / `shell,v2`) are bridged through it exactly as on USB.
type PersistentTcpConnection = PersistentConnection<TcpTransport>;

/// Channel depth for the `subscribe_changes` snapshot stream. Small: only the
/// latest snapshot matters, and the consumer (one `host:track-devices` client)
/// drains promptly.
const CHANGES_CHANNEL_SIZE: usize = 8;

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
}

/// Deprecated former name of [`DefaultDeviceBackend`]. The default backend now
/// also tracks TCP/IP devices, so the `Usb`-specific name no longer fits; this
/// alias keeps existing `use adb_client::server::UsbDeviceBackend` compiling.
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
        let tcp_serials: Vec<String> = self.tcp_devices.lock().await.keys().cloned().collect();
        merge_device_sets(Self::enumerate_usb(), tcp_serials)
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
}

impl DeviceBackend for DefaultDeviceBackend {
    async fn list_devices(&self) -> Vec<DeviceEntry> {
        self.enumerate_all().await
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
                let tcp_serials: Vec<String> = tcp.lock().await.keys().cloned().collect();
                merge_device_sets(Self::enumerate_usb(), tcp_serials)
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

    async fn open_reverse(&self, serial: &str, remote: &str, local: &str) -> Result<()> {
        self.reverse_engine(serial).await?.open(remote, local).await
    }

    async fn reverse_remove(&self, serial: &str, remote: &str) -> Result<()> {
        self.reverse_engine(serial).await?.remove(remote).await
    }

    async fn reverse_remove_all(&self, serial: &str) -> Result<()> {
        self.reverse_engine(serial).await?.remove_all().await
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
            let mut map = self.tcp_devices.lock().await;
            let n = map.len();
            map.clear();
            return Ok(format!("disconnected everything ({n} device(s))"));
        }

        let (_socket, serial) = Self::resolve_tcp_target(addr)?;
        if self.tcp_devices.lock().await.remove(&serial).is_some() {
            tracing::info!("host:disconnect removed TCP/IP device {serial}");
            Ok(format!("disconnected {serial}"))
        } else {
            Err(RustADBError::ADBRequestFailed(format!(
                "no such device {serial}"
            )))
        }
    }
}

/// Merge the USB device entries with one [`DeviceEntry`] per `host:connect`ed
/// TCP serial into the single unified device set. Pure (sans-io) so the merge
/// shape is unit-tested without hardware; TCP devices use the default
/// `DeviceState::Device` (a tracked TCP device is, by construction, connected).
fn merge_device_sets(
    usb: Vec<DeviceEntry>,
    tcp_serials: impl IntoIterator<Item = String>,
) -> Vec<DeviceEntry> {
    let mut all = usb;
    all.extend(tcp_serials.into_iter().map(DeviceEntry::new));
    all
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
        let tcp_serials = ["10.0.0.5:5555".to_string(), "10.0.0.6:5555".to_string()];
        let merged = merge_device_sets(usb, tcp_serials.iter().cloned());
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
}
