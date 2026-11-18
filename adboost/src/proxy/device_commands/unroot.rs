use crate::{
    Result, RustADBError,
    models::{ADBLocalCommand, RootOutcome, RootState},
    proxy::ADBProxyDevice,
};

use super::response::RootDirection;

impl ADBProxyDevice {
    /// Restart adb daemon without root permissions.
    ///
    /// Reads and classifies the device receipt; a refusal is reported as
    /// [`RustADBError::RootRefused`]. Use [`Self::unroot_with_outcome`] for the
    /// full structured result.
    ///
    /// # Errors
    ///
    /// [`RustADBError::RootRefused`] when the device refuses the mode change.
    pub async fn unroot(&mut self) -> Result<()> {
        let outcome = self.unroot_with_outcome().await?;
        match outcome.state {
            RootState::Refused => Err(RustADBError::RootRefused { raw: outcome.raw }),
            RootState::Unknown => {
                // Same compatibility stance as `root()`: unknown vendor wording
                // is surfaced by the structured API, not turned into a regression.
                tracing::warn!("unrecognized adb unroot receipt: {:?}", outcome.raw);
                Ok(())
            }
            RootState::Restarting | RootState::AlreadyRoot => Ok(()),
        }
    }

    /// Request unroot and return the classified device receipt.
    ///
    /// As with [`Self::root_with_outcome`], adboost reports the receipt only and
    /// does not wait for the device to reconnect.
    ///
    /// # Errors
    ///
    /// Propagates transport errors.
    pub async fn unroot_with_outcome(&mut self) -> Result<RootOutcome> {
        self.root_mode_outcome(ADBLocalCommand::Unroot, RootDirection::Unroot)
            .await
    }
}
