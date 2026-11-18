use crate::{
    Result,
    models::{ADBCommand, ADBLocalCommand, RemountInfo, RemountOutcome},
    proxy::ADBProxyDevice,
};

use super::response;

impl ADBProxyDevice {
    /// Remount the device filesystem as read-write.
    ///
    /// Compatibility wrapper over [`Self::remount_with_outcome`] returning only
    /// the parsed per-partition detail.
    pub async fn remount(&mut self) -> Result<Vec<RemountInfo>> {
        self.remount_with_outcome()
            .await
            .map(|outcome| outcome.info)
    }

    /// Remount the device filesystem and return the full device output.
    ///
    /// The reply is read to EOF (bounded) instead of a single 1024-byte read, so
    /// multi-line `Using <path> for <mode>` output is never truncated. Success is
    /// accepted when the device reports `remount succeeded` **or** at least one
    /// `Using` line, and no failure marker (`remount failed`, `not running as
    /// root`, `permission denied`, …) is present. Otherwise
    /// [`RustADBError::RemountError`] carries the raw text.
    ///
    /// [`RustADBError::RemountError`]: crate::RustADBError::RemountError
    ///
    /// # Errors
    ///
    /// [`RustADBError::RemountError`] when the device reports a failure; transport
    /// errors are propagated unchanged.
    pub async fn remount_with_outcome(&mut self) -> Result<RemountOutcome> {
        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::Remount))
            .await?;

        let raw = {
            let connection = self.transport.get_raw_connection()?;
            response::read_reply(connection, None).await?
        };
        let info = RemountInfo::from_str_response(&raw)?;

        tracing::info!(
            "remount succeeded ({} partition entries): {:?}",
            info.len(),
            raw
        );
        Ok(RemountOutcome { info, raw })
    }
}
