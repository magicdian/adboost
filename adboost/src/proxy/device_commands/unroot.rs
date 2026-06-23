use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Restart adb daemon without root permissions
    pub async fn unroot(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::Unroot), false)
            .await
            .map(|_| ())
    }
}
