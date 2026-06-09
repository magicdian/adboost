use std::path::Path;

use tokio::io::AsyncReadExt;

use crate::{
    Result,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
        commands::utils::MessageWriter, message_commands::MessageCommand,
    },
    models::ADBLocalCommand,
    utils::check_extension_is_apk,
};

const INSTALL_BUFFER_SIZE: usize = 65535;

impl<T: ADBMessageTransport> ADBMessageDevice<T> {
    pub(crate) async fn install(
        &mut self,
        apk_path: &(dyn AsRef<Path> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        let mut apk_file = tokio::fs::File::open(apk_path.as_ref()).await?;

        check_extension_is_apk(apk_path.as_ref())?;

        let file_size = apk_file.metadata().await?.len();

        let mut session = self
            .open_session(&ADBLocalCommand::Install(
                file_size,
                user.map(ToString::to_string),
            ))
            .await?;

        {
            // Read data from apk_file and write it to the underlying session
            let mut writer = MessageWriter::new(&mut session);
            let mut buffer = vec![0u8; INSTALL_BUFFER_SIZE].into_boxed_slice();
            loop {
                let size = apk_file.read(&mut buffer).await?;
                if size == 0 {
                    break;
                }
                writer.write(&buffer[..size]).await?;
            }
        }

        let final_status = session.get_transport_mut().read_message().await?;

        match final_status.into_payload().as_slice() {
            b"Success\n" => {
                log::info!(
                    "APK file {} successfully installed",
                    apk_path.as_ref().display()
                );
                self.get_transport_mut()
                    .read_message()
                    .await?
                    .assert_command(MessageCommand::Clse)?;
                Ok(())
            }
            d => Err(crate::RustADBError::ADBRequestFailed(String::from_utf8(
                d.to_vec(),
            )?)),
        }
    }
}
