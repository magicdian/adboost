use crate::{
    Result,
    emulator::{ADBEmulatorCommand, ADBEmulatorDevice},
};

impl ADBEmulatorDevice {
    /// Send a SMS to this emulator with given content with given phone number
    pub async fn send_sms(&mut self, phone_number: &str, content: &str) -> Result<()> {
        let _ = self
            .connect()
            .await?
            .send_command(&ADBEmulatorCommand::Sms(
                phone_number.to_string(),
                content.to_string(),
            ))
            .await?;
        Ok(())
    }
}
