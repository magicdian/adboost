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
    MultiplexedSession, PersistentUsbConnection, ShellV2Session, SyncSession,
    find_all_connected_adb_devices,
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
}

impl UsbDeviceBackend {
    /// A backend using the default ADB key location.
    #[must_use]
    pub fn new() -> Self {
        Self::default()
    }

    /// A backend using a specific ADB private-key path for every device.
    #[must_use]
    pub fn with_private_key(private_key_path: PathBuf) -> Self {
        Self {
            conns: Mutex::new(HashMap::new()),
            private_key_path: Some(private_key_path),
        }
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

    async fn capabilities(&self) -> BackendCapabilities {
        // The persistent USB connection genuinely bridges both SYNC v1 and
        // shell-v2 (see `open_sync_session` / `open_shell_v2` below), so we
        // advertise both — the frontend turns these into honest
        // `sync_v2` / `shell_v2` host-feature claims.
        BackendCapabilities {
            sync: true,
            shell_v2: true,
        }
    }

    async fn open_sync_session(&self, serial: &str) -> Result<SyncSession> {
        let conn = self.get_or_open(serial).await?;
        conn.open_sync_session().await
    }

    async fn open_shell_v2(&self, serial: &str, cmd: &str) -> Result<ShellV2Session> {
        let conn = self.get_or_open(serial).await?;
        conn.open_shell_v2(cmd).await
    }
}
