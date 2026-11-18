use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBLocalCommand, RootOutcome, RootState},
    proxy::ADBProxyDevice,
};

use super::response::{self, RootDirection};

impl ADBProxyDevice {
    /// Restart adb daemon with root permissions.
    ///
    /// Reads the device receipt instead of discarding it, and reports a refusal
    /// (`adbd cannot run as root in production builds`) as
    /// [`RustADBError::RootRefused`]. Use [`Self::root_with_outcome`] when the
    /// caller needs the full classification.
    ///
    /// # Errors
    ///
    /// [`RustADBError::RootRefused`] when the device refuses root.
    pub async fn root(&mut self) -> Result<()> {
        let outcome = self.root_with_outcome().await?;
        match outcome.state {
            RootState::Refused => Err(RustADBError::RootRefused { raw: outcome.raw }),
            RootState::Unknown => {
                // Do not turn unknown vendor wording into a hard failure on the
                // legacy API; the structured API exposes it explicitly.
                tracing::warn!("unrecognized adb root receipt: {:?}", outcome.raw);
                Ok(())
            }
            RootState::Restarting | RootState::AlreadyRoot => Ok(()),
        }
    }

    /// Request root and return the classified device receipt.
    ///
    /// adboost reports **facts only**: it does not wait for the device to
    /// reconnect after `Restarting`. That bounded wait is caller policy (e.g. a
    /// daemon), because only the caller knows its reconnect budget.
    ///
    /// # Errors
    ///
    /// Propagates transport errors; a device refusal is returned as
    /// [`RootOutcome::state`] `= RootState::Refused`, not as an error, so callers
    /// can branch on the full classification.
    pub async fn root_with_outcome(&mut self) -> Result<RootOutcome> {
        self.root_mode_outcome(ADBLocalCommand::Root, RootDirection::Root)
            .await
    }

    /// Shared request/read/classify flow for `root` and `unroot`.
    pub(super) async fn root_mode_outcome(
        &mut self,
        command: ADBLocalCommand,
        direction: RootDirection,
    ) -> Result<RootOutcome> {
        self.set_serial_transport().await?;
        self.transport
            .send_adb_request(&ADBCommand::Local(command))
            .await?;

        let raw = {
            let connection = self.transport.get_raw_connection()?;
            response::read_reply(connection, None).await?
        };
        let state = response::classify_root_receipt(&raw, direction);
        tracing::info!("adb {direction:?} receipt classified as {state:?}: {raw:?}");

        Ok(RootOutcome { state, raw })
    }
}
