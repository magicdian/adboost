use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Restart adb daemon with root permissions
    pub async fn root(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::Root), false)
            .await
            .map(|_| ())
    }
}
