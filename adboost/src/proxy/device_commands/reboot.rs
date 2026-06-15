use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand, RebootType},
    proxy::ADBProxyDevice,
};

impl ADBProxyDevice {
    /// Reboots the device
    pub async fn reboot(&mut self, reboot_type: RebootType) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .proxy_connection(
                &ADBCommand::Local(ADBLocalCommand::Reboot(reboot_type)),
                false,
            )
            .await
            .map(|_| ())
    }
}
