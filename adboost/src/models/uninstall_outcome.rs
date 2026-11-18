/// Structured result of a successful package uninstall.
///
/// Failure carries a typed [`RustADBError::UninstallFailed`] instead.
///
/// [`RustADBError::UninstallFailed`]: crate::RustADBError::UninstallFailed
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct UninstallOutcome {
    /// Always `true` for a returned outcome.
    pub success: bool,
    /// Raw device text (`Success` or the failure reason).
    pub raw: String,
}
