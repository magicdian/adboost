use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Set adb daemon to usb mode
    pub async fn usb(&mut self) -> Result<()> {
        self.set_serial_transport().await?;
        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::Usb), false)
            .await
            .map(|_| ())
    }
}
