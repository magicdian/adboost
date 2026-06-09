use crate::{
    RebootType, Result,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
        message_commands::MessageCommand,
    },
    models::ADBLocalCommand,
};

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    pub(crate) async fn reboot(&mut self, reboot_type: RebootType) -> Result<()> {
        self.open_session(&ADBLocalCommand::Reboot(reboot_type))
            .await?;

        self.get_transport_mut()
            .read_message()
            .await?
            .assert_command(MessageCommand::Okay)
    }
}
