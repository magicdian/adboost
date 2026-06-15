use crate::{
    Result,
    emulator::{ADBEmulatorCommand, ADBEmulatorDevice},
};

impl ADBEmulatorDevice {
    /// Send a SMS to this emulator with given content with given phone number
    pub async fn rotate(&mut self) -> Result<()> {
        let _ = self
            .connect()
            .await?
            .send_command(&ADBEmulatorCommand::Rotate)
            .await?;
        Ok(())
    }
}
