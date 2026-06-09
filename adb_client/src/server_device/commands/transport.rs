use crate::{
    Result,
    models::{ADBCommand, ADBHostCommand},
    server_device::ADBServerDevice,
};

impl ADBServerDevice {
    /// Asks ADB server to switch the connection to either the device or emulator connect to/running on the host.
    /// Will fail if there is more than one such device/emulator available.
    pub async fn transport_any(&mut self) -> Result<()> {
        self.connect()
            .await?
            .proxy_connection(&ADBCommand::Host(ADBHostCommand::TransportAny), false)
            .await
            .map(|_| ())
    }
}
