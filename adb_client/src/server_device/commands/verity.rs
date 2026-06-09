use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    server_device::ADBServerDevice,
};

impl ADBServerDevice {
    /// Disable verity on the device
    pub async fn disable_verity(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::DisableVerity))
            .await
    }

    /// Enable verity on the device
    pub async fn enable_verity(&mut self) -> Result<()> {
        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::EnableVerity))
            .await
    }
}
