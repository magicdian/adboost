use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Forward socket connection.
    ///
    /// `forward` is a **device-pinned host service**: it is scoped to this device
    /// by a `host-serial:`/`host-transport-id:` prefix (via [`DeviceSelector`]),
    /// NOT by a preceding `host:transport:` switch. Issuing a bare `host:forward`
    /// after a transport switch made the server auto-select a device and fail with
    /// `more than one device/emulator` once ≥2 were attached.
    ///
    /// [`DeviceSelector`]: crate::models::DeviceSelector
    pub async fn forward(&mut self, remote: String, local: String) -> Result<()> {
        let cmd = ADBHostCommand::Forward {
            selector: self.selector(),
            local,
            remote,
        };
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(cmd), false)
            .await
            .map(|_| ())
    }

    /// Remove a previously applied forward rule by its local endpoint.
    ///
    /// Device-pinned like [`Self::forward`] (`{selector}killforward:<local>`).
    pub async fn forward_remove(&mut self, local: String) -> Result<()> {
        let cmd = ADBHostCommand::KillForward {
            selector: self.selector(),
            local,
        };
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(cmd), false)
            .await
            .map(|_| ())
    }

    /// Remove all previously applied forward rules.
    ///
    /// NOT device-scoped: AOSP's `killforward-all` is process-global (native
    /// `adb -s <serial> forward --remove-all` still sends the bare
    /// `host:killforward-all`), so this needs no device selector.
    pub async fn forward_remove_all(&mut self) -> Result<()> {
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::KillForwardAll), false)
            .await
            .map(|_| ())
    }
}
