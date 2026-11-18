/// Structured result of a successful install.
///
/// Failure carries a typed [`RustADBError::InstallFailed`] instead, so an
/// `InstallOutcome` always describes an installed package.
///
/// [`RustADBError::InstallFailed`]: crate::RustADBError::InstallFailed
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct InstallOutcome {
    /// Always `true` for a returned outcome; kept for API symmetry with the
    /// device's own `Success`/`Failure` vocabulary.
    pub success: bool,
    /// Package name when the device reply could be parsed for one.
    ///
    /// `cmd package install` normally reports no package name, so this is
    /// `None` for the standard path.
    pub package: Option<String>,
    /// Raw device text (success or failure reason).
    pub raw: String,
}
