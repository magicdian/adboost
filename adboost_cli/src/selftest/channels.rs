//! Device discovery and the two automated test *channels*.
//!
//! A channel is a way to reach a device:
//!
//! - **USB-direct** ([`ADBUSBDevice`]) — adboost talks straight to the device
//!   over USB (no adb server in the path). Validates the library's own stack.
//! - **Through-server** ([`ADBProxyDevice`]) — adboost stands up its OWN ADB
//!   server frontend (on an ephemeral loopback port, so it never disturbs a
//!   real `:5037`) and connects to it as a client. Validates the server
//!   frontend end-to-end against the same device.
//!
//! Both run the SAME case suite ([`super::cases`]) so any behavioral divergence
//! between the direct path and the server path surfaces as a failed case.

use std::net::{SocketAddr, SocketAddrV4};
use std::sync::Arc;

use adb_client::server::{AdbServerFrontend, UsbDeviceBackend};
use adb_client::usb::{ADBDeviceInfo, find_all_connected_adb_devices};

/// One discovered USB device, classified for suite selection.
#[derive(Clone, Debug)]
pub struct DiscoveredDevice {
    /// The USB serial — `None` for a device that exposes no serial (it cannot be
    /// addressed by the host protocol and is reported but not exercised).
    pub serial: Option<String>,
    /// Device description (for human-readable listing).
    pub description: String,
}

impl From<&ADBDeviceInfo> for DiscoveredDevice {
    fn from(info: &ADBDeviceInfo) -> Self {
        Self {
            serial: info.serial.clone(),
            description: info.device_description.clone(),
        }
    }
}

/// Enumerate connected ADB USB devices. Empty vec ⇒ nothing connected.
///
/// # Errors
///
/// Returns the underlying enumeration error if the USB stack cannot be queried.
pub fn discover_devices() -> Result<Vec<DiscoveredDevice>, String> {
    find_all_connected_adb_devices()
        .map(|devs| devs.iter().map(DiscoveredDevice::from).collect())
        .map_err(|e| format!("USB device enumeration failed: {e}"))
}

/// A running in-process adboost ADB server bound to an ephemeral loopback port,
/// for the through-server channel. The accept loop runs on a background task
/// that is aborted on drop, freeing the port.
pub struct InProcessServer {
    addr: SocketAddrV4,
    task: tokio::task::JoinHandle<()>,
    /// Handle to the backend whose cached device connections must be gracefully
    /// closed (connection-level CLSE flushed) before the process tears down — see
    /// [`InProcessServer::shutdown`].
    backend: Arc<UsbDeviceBackend>,
}

impl InProcessServer {
    /// Bind an adboost server frontend on `127.0.0.1:0` (OS-assigned port) over
    /// the default USB backend, and start serving on a background task.
    ///
    /// Using port 0 (not `:5037`) is deliberate: the self-test never contends
    /// with or kills a real adb server.
    ///
    /// # Errors
    ///
    /// Returns an error if the ephemeral port cannot be bound.
    pub async fn start() -> Result<Self, String> {
        // Bind synchronously first so we learn the assigned port before serving.
        let listener = tokio::net::TcpListener::bind(SocketAddr::from(([127, 0, 0, 1], 0)))
            .await
            .map_err(|e| format!("cannot bind in-process server: {e}"))?;
        let addr = match listener
            .local_addr()
            .map_err(|e| format!("cannot read server addr: {e}"))?
        {
            SocketAddr::V4(v4) => v4,
            SocketAddr::V6(_) => {
                return Err("in-process server bound a non-IPv4 address".to_string());
            }
        };
        // The frontend binds its own listener; we only needed `listener` to
        // reserve+discover the port, so drop it before the frontend rebinds.
        drop(listener);

        let backend = Arc::new(UsbDeviceBackend::new());
        // Keep a handle to gracefully close the backend's cached device
        // connections on shutdown (the frontend takes ownership of its own clone).
        let shutdown_backend = Arc::clone(&backend);
        let frontend = AdbServerFrontend::builder(backend)
            .addr(SocketAddr::V4(addr))
            .build();
        let task = tokio::spawn(async move {
            if let Err(e) = frontend.serve().await {
                tracing::warn!("in-process selftest server exited: {e}");
            }
        });

        // Give the frontend a moment to rebind the port before clients connect.
        tokio::time::sleep(std::time::Duration::from_millis(150)).await;

        Ok(Self {
            addr,
            task,
            backend: shutdown_backend,
        })
    }

    /// The loopback address clients should connect to.
    #[must_use]
    pub fn addr(&self) -> SocketAddrV4 {
        self.addr
    }

    /// Gracefully shut the server down: flush a connection-level CLSE to every
    /// cached device connection (while the writer tasks are still alive), then
    /// abort the accept loop and free the port.
    ///
    /// Prefer this over relying on `Drop` (which can only `abort` the accept task,
    /// not `.await` the backend's per-connection CLSE flush). Without it the device
    /// connections are torn down at process exit when their writer tasks may
    /// already be gone, leaving orphaned streams that make the next run's
    /// `usb_direct` CNXN hit a stale CLSE.
    pub async fn shutdown(self) {
        self.backend.shutdown().await;
        self.task.abort();
    }
}

impl Drop for InProcessServer {
    fn drop(&mut self) {
        // Fallback only: graceful teardown should go through `shutdown().await`,
        // which flushes per-connection CLSEs first. Drop cannot `.await`, so it can
        // only stop the accept loop.
        self.task.abort();
    }
}
