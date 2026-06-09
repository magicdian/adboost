use crate::{
    Result,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
        message_commands::MessageCommand,
    },
    models::ADBLocalCommand,
};

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    pub(crate) async fn enable_verity(&mut self) -> Result<()> {
        self.open_session(&ADBLocalCommand::EnableVerity).await?;

        self.get_transport_mut()
            .read_message()
            .await?
            .assert_command(MessageCommand::Okay)
    }

    pub(crate) async fn disable_verity(&mut self) -> Result<()> {
        self.open_session(&ADBLocalCommand::DisableVerity).await?;

        self.get_transport_mut()
            .read_message()
            .await?
            .assert_command(MessageCommand::Okay)
    }
}
