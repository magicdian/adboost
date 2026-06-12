use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Reconnect device
    pub async fn reconnect(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(&ADBCommand::Local(ADBLocalCommand::Reconnect), false)
            .await
            .map(|_| ())
    }
}
