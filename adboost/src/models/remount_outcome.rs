use crate::models::RemountInfo;

/// Structured result of a `remount` request.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemountOutcome {
    /// Parsed `Using <path> for <mode>` success lines (empty when the device
    /// reported plain success without per-partition detail).
    pub info: Vec<RemountInfo>,
    /// Full device output, retained for diagnostics and vendor wording.
    pub raw: String,
}
