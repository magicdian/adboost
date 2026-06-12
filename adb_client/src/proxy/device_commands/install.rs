use std::path::Path;

use tokio::io::{AsyncReadExt, AsyncWriteExt};

use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand},
    proxy::ADBProxyDevice,
    utils::check_extension_is_apk,
};

const INSTALL_BUFFER_SIZE: usize = 65535;

impl ADBProxyDevice {
    /// Install an APK on device
    pub async fn install<P: AsRef<Path>>(&mut self, apk_path: P, user: Option<&str>) -> Result<()> {
        let mut apk_file = tokio::fs::File::open(&apk_path).await?;

        check_extension_is_apk(&apk_path)?;

        let file_size = apk_file.metadata().await?.len();

        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::Install(
                file_size,
                user.map(ToString::to_string),
            )))
            .await?;

        {
            let raw_connection = self.transport.get_raw_connection()?;
            let mut buffer = vec![0u8; INSTALL_BUFFER_SIZE].into_boxed_slice();
            loop {
                let size = apk_file.read(&mut buffer).await?;
                if size == 0 {
                    break;
                }
                raw_connection.write_all(&buffer[..size]).await?;
            }
        }

        let mut data = [0; 1024];
        let read_amount = self.transport.get_raw_connection()?.read(&mut data).await?;

        match &data[0..read_amount] {
            b"Success\n" => {
                tracing::info!(
                    "APK file {} successfully installed",
                    apk_path.as_ref().display()
                );
                Ok(())
            }
            d => Err(crate::RustADBError::ADBRequestFailed(String::from_utf8(
                d.to_vec(),
            )?)),
        }
    }
}
