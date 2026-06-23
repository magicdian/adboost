//! [`ForwardHandle`] — the caller-facing active-cleanup API for `forward` /
//! `reverse` rules.
//!
//! [`AdbServerFrontend::serve`](super::AdbServerFrontend::serve) consumes
//! `self`, so once the server is running a caller can no longer reach the
//! frontend to release rules. `ForwardHandle` is the durable, clone-friendly
//! grip on the two pieces of state cleanup needs — the server-global
//! [`ForwardRegistry`] and the [`DeviceBackend`] (which owns reverse rules) —
//! obtained via [`AdbServerFrontend::handle`](super::AdbServerFrontend::handle)
//! *before* serving and kept afterwards.
//!
//! It backs all three release paths uniformly (one method clears a serial's
//! forward listeners **and** its reverse rules):
//! - [`OnDisconnect::Retain`](super::OnDisconnect::Retain): the caller releases
//!   on its own schedule.
//! - [`OnDisconnect::Notify`](super::OnDisconnect::Notify): the callback calls
//!   these methods.
//! - ad-hoc: any caller wanting to drop a device's rules without waiting for a
//!   disconnect.

use std::sync::Arc;

use super::backend::DeviceBackend;
use super::forward::ForwardRegistry;

/// A clone-friendly handle for releasing a device's `forward` + `reverse` rules.
///
/// Cloning is cheap (two `Arc` bumps); every clone refers to the same registry
/// and backend as the frontend it came from. Holding a handle does **not** keep
/// the server's accept loop alive — it only shares the rule state.
pub struct ForwardHandle<B: DeviceBackend> {
    forwards: Arc<ForwardRegistry>,
    backend: Arc<B>,
}

impl<B: DeviceBackend> Clone for ForwardHandle<B> {
    fn clone(&self) -> Self {
        Self {
            forwards: Arc::clone(&self.forwards),
            backend: Arc::clone(&self.backend),
        }
    }
}

impl<B: DeviceBackend> ForwardHandle<B> {
    /// Construct a handle over the frontend's shared state. Crate-internal: the
    /// only blessed source is
    /// [`AdbServerFrontend::handle`](super::AdbServerFrontend::handle).
    pub(super) fn new(forwards: Arc<ForwardRegistry>, backend: Arc<B>) -> Self {
        Self { forwards, backend }
    }

    /// Release every `forward` listener and `reverse` rule for `serial`.
    ///
    /// Aborts the serial's host-side forward accept loops (freeing their local
    /// ports) and clears its reverse rules in the backend. Idempotent: a serial
    /// with no rules is a no-op. Returns the number of forward rules removed
    /// (reverse removal is best-effort and not counted, since the backend owns
    /// that bookkeeping).
    ///
    /// Reverse removal failures (e.g. the backend never had reverse rules for
    /// this serial, or its connection is already gone) are logged and swallowed:
    /// disconnect cleanup must not fail just because one half had nothing to do.
    pub async fn release(&self, serial: &str) -> usize {
        let removed = self.forwards.remove_by_serial(serial).await;
        if let Err(e) = self.backend.release_reverse(serial).await {
            tracing::debug!(
                serial,
                "ForwardHandle::release: reverse cleanup reported nothing to do: {e}"
            );
        }
        removed
    }

    /// Release **all** `forward` listeners and every known serial's `reverse`
    /// rules. Used for a full teardown when individual serials are not the unit
    /// of interest.
    ///
    /// Reverse rules are keyed by serial in the backend, so this fans out over
    /// the serials currently present in the forward registry *before* clearing
    /// it. A reverse-only serial (one that has reverse rules but no forward
    /// rule) is not visible here; release it explicitly via [`Self::release`].
    pub async fn release_all(&self) {
        let serials = self.forwards.serials().await;
        self.forwards.remove_all().await;
        for serial in serials {
            if let Err(e) = self.backend.release_reverse(&serial).await {
                tracing::debug!(
                    serial,
                    "ForwardHandle::release_all: reverse cleanup reported nothing to do: {e}"
                );
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::Result;
    use crate::models::ADBLocalCommand;
    use crate::server::backend::DeviceEntry;
    use std::sync::Mutex;
    use tokio::sync::mpsc;

    /// Minimal hardware-free backend that records which serials had their reverse
    /// rules released, so the handle's forward+reverse fan-out is observable.
    #[derive(Default)]
    struct RecordingBackend {
        released_reverse: Arc<Mutex<Vec<String>>>,
    }

    impl DeviceBackend for RecordingBackend {
        async fn list_devices(&self) -> Vec<DeviceEntry> {
            Vec::new()
        }
        async fn subscribe_changes(&self) -> mpsc::Receiver<Vec<DeviceEntry>> {
            let (_tx, rx) = mpsc::channel(1);
            rx
        }
        async fn open_local_service(
            &self,
            _serial: &str,
            _cmd: &ADBLocalCommand,
        ) -> Result<crate::usb::MultiplexedSession> {
            unimplemented!("not exercised")
        }
        // The disconnect path calls `release_reverse`; record the serial.
        async fn release_reverse(&self, serial: &str) -> Result<()> {
            self.released_reverse
                .lock()
                .expect("test lock")
                .push(serial.to_owned());
            Ok(())
        }
    }

    /// Build a registry pre-populated with forward rules. `(local_port, serial)`
    /// pairs; the listener task is a no-op stand-in.
    async fn registry_with(rules: &[(u16, &str)]) -> Arc<ForwardRegistry> {
        let reg = Arc::new(ForwardRegistry::default());
        for (port, serial) in rules {
            reg.insert(*port, 1, (*serial).to_string(), tokio::spawn(async {}))
                .await;
        }
        reg
    }

    #[tokio::test]
    async fn release_drops_only_that_serial_forward_and_its_reverse() {
        let backend = Arc::new(RecordingBackend::default());
        let log = Arc::clone(&backend.released_reverse);
        let reg = registry_with(&[(8000, "serialA"), (8001, "serialA"), (9000, "serialB")]).await;
        let handle = ForwardHandle::new(Arc::clone(&reg), backend);

        let n = handle.release("serialA").await;
        assert_eq!(n, 2, "both serialA forward rules released");
        assert!(!reg.contains(8000).await && !reg.contains(8001).await);
        assert!(reg.contains(9000).await, "serialB forward must survive");
        assert_eq!(
            log.lock().expect("test lock").as_slice(),
            ["serialA"],
            "reverse released for exactly the disconnected serial"
        );
    }

    #[tokio::test]
    async fn release_all_clears_forwards_and_fans_reverse_over_serials() {
        let backend = Arc::new(RecordingBackend::default());
        let log = Arc::clone(&backend.released_reverse);
        let reg = registry_with(&[(8000, "serialA"), (8001, "serialA"), (9000, "serialB")]).await;
        let handle = ForwardHandle::new(Arc::clone(&reg), backend);

        handle.release_all().await;
        assert!(
            reg.list().await.is_empty(),
            "release_all clears every forward rule"
        );
        // Reverse fan-out hits each distinct serial once (registry dedups).
        let mut released = log.lock().expect("test lock").clone();
        released.sort();
        assert_eq!(
            released,
            ["serialA", "serialB"],
            "reverse released once per distinct serial that had a forward rule"
        );
    }
}
