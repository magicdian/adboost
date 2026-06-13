//! The bundled, zero-config USB device backend.
//!
//! [`UsbDeviceBackend`] is the default [`DeviceBackend`]: it enumerates ADB USB
//! devices, opens local services over [`PersistentUsbConnection`], and reports
//! device-set changes via nusb hotplug. It is a thin wrapper — all device-side
//! transport, multiplexing, and flow control come from the existing persistent
//! connection; this type only maps serials to connections and caches them.

use std::collections::HashMap;
use std::path::PathBuf;
use std::sync::Arc;

use futures_util::StreamExt;
use tokio::sync::{Mutex, mpsc};

use super::backend::{BackendCapabilities, DeviceBackend, DeviceEntry};
use crate::models::ADBLocalCommand;
use crate::usb::{
    MultiplexedSession, PersistentUsbConnection, ReverseEngine, ReversePolicy, ShellV2Session,
    SyncSession, find_all_connected_adb_devices,
};
use crate::{Result, RustADBError};

/// Channel depth for the `subscribe_changes` snapshot stream. Small: only the
/// latest snapshot matters, and the consumer (one `host:track-devices` client)
/// drains promptly.
const CHANGES_CHANNEL_SIZE: usize = 8;

/// Default [`DeviceBackend`] over USB: serial-keyed [`PersistentUsbConnection`]
/// cache + nusb hotplug change stream. No custom discovery/relay/auth — inject
/// your own `DeviceBackend` for that.
#[derive(Default)]
pub struct UsbDeviceBackend {
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
}

impl UsbDeviceBackend {
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
    fn enumerate() -> Vec<DeviceEntry> {
        match find_all_connected_adb_devices() {
            Ok(devices) => devices
                .into_iter()
                .filter_map(|d| d.serial.map(DeviceEntry::new))
                .collect(),
            Err(e) => {
                tracing::warn!("UsbDeviceBackend: device enumeration failed: {e}");
                Vec::new()
            }
        }
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

impl DeviceBackend for UsbDeviceBackend {
    async fn list_devices(&self) -> Vec<DeviceEntry> {
        Self::enumerate()
    }

    async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
        let (tx, rx) = mpsc::channel(CHANGES_CHANNEL_SIZE);

        // Spawn a watcher that pushes a full snapshot on every hotplug event.
        // We start the watch BEFORE the initial enumeration so a device added
        // in the gap is not missed (nusb's documented ordering guidance), then
        // send the initial snapshot. If hotplug is unavailable on this platform
        // / setup, fall back to sending one snapshot and closing.
        match nusb::watch_devices() {
            Ok(watch) => {
                tokio::spawn(async move {
                    // Initial snapshot.
                    if tx.send(Self::enumerate()).await.is_err() {
                        return; // receiver already gone
                    }
                    let mut watch = watch;
                    // Each hotplug event (connect/disconnect) → re-enumerate and
                    // push a fresh full snapshot. Coalescing is unnecessary: the
                    // consumer only cares about the latest set.
                    while watch.next().await.is_some() {
                        if tx.send(Self::enumerate()).await.is_err() {
                            break; // receiver dropped → stop watching
                        }
                    }
                    tracing::debug!("UsbDeviceBackend: hotplug watch ended");
                });
            }
            Err(e) => {
                tracing::warn!(
                    "UsbDeviceBackend: hotplug unavailable ({e}); sending a single device snapshot"
                );
                tokio::spawn(async move {
                    let _ = tx.send(Self::enumerate()).await;
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
                    "UsbDeviceBackend: unsupported local service: {other}"
                )));
            }
        }
        let conn = self.get_or_open(serial).await?;
        conn.open_session(cmd).await
    }

    async fn open_sync_session(&self, serial: &str) -> Result<SyncSession> {
        let conn = self.get_or_open(serial).await?;
        conn.open_sync_session().await
    }

    async fn open_shell_v2(&self, serial: &str, cmd: &str) -> Result<ShellV2Session> {
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
}
