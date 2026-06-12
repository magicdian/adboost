use crate::{
    Result,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
    },
    models::ADBLocalCommand,
};

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    pub(crate) async fn uninstall(
        &mut self,
        package_name: &(dyn AsRef<str> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.open_session(&ADBLocalCommand::Uninstall(
            package_name.as_ref().to_string(),
            user.map(ToString::to_string),
        ))
        .await?;

        let final_status = self.get_transport_mut().read_message().await?;

        match final_status.into_payload().as_slice() {
            b"Success\n" => {
                tracing::info!("Package {} successfully uninstalled", package_name.as_ref());
                Ok(())
            }
            d => Err(crate::RustADBError::ADBRequestFailed(String::from_utf8(
                d.to_vec(),
            )?)),
        }
    }
}
