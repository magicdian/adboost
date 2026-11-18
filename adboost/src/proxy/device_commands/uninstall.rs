use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBLocalCommand, UninstallOutcome},
    proxy::ADBProxyDevice,
};

use super::response::{self, ReplyKind};

impl ADBProxyDevice {
    /// Uninstall a package from device.
    ///
    /// Compatibility wrapper over [`Self::uninstall_with_outcome`] that discards
    /// the structured result.
    pub async fn uninstall(&mut self, package_name: &str, user: Option<&str>) -> Result<()> {
        self.uninstall_with_outcome(package_name, user)
            .await
            .map(|_| ())
    }

    /// Uninstall a package and return the structured device result.
    ///
    /// The reply is read to EOF (not a single fixed-size read) and classified:
    /// `Success` → [`UninstallOutcome`], `Failure [REASON]` →
    /// [`RustADBError::UninstallFailed`], anything else → the same error with
    /// `reason = "unknown response"`.
    ///
    /// # Errors
    ///
    /// Propagates I/O and transport errors and
    /// [`RustADBError::UninstallFailed`].
    pub async fn uninstall_with_outcome(
        &mut self,
        package_name: &str,
        user: Option<&str>,
    ) -> Result<UninstallOutcome> {
        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::Uninstall(
                package_name.to_string(),
                user.map(ToString::to_string),
            )))
            .await?;

        let raw = {
            let connection = self.transport.get_raw_connection()?;
            response::read_reply(connection, None).await?
        };

        match response::parse_success_or_failure(&raw) {
            ReplyKind::Success => {
                tracing::info!("Package {package_name} successfully uninstalled");
                Ok(UninstallOutcome { success: true, raw })
            }
            ReplyKind::Failure(reason) => Err(RustADBError::UninstallFailed { reason, raw }),
            ReplyKind::Unknown => Err(RustADBError::UninstallFailed {
                reason: "unknown response".to_string(),
                raw,
            }),
        }
    }
}
