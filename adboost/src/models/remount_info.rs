use std::{str::FromStr, sync::LazyLock};

use regex::Regex;

use crate::{Result, RustADBError};

static REMOUNT_REGEX: LazyLock<Regex> = LazyLock::new(|| {
    Regex::new(r"^Using\s+(?P<path>\S+)\s+for\s+(?P<mode>\S+)$").expect("Invalid remount regex")
});

/// Device-side failure markers, matched case-insensitively.
///
/// The legacy parser required the response to end with the exact suffix
/// `"remount succeeded"`, which rejected vendor/adbd versions that word the
/// result differently. Failure wording, by contrast, is a reliable negative
/// signal, so success is inferred from "a success marker or per-partition
/// `Using` lines, and no failure marker".
const REMOUNT_FAILURE_MARKERS: [&str; 6] = [
    "remount failed",
    "failed to remount",
    "not running as root",
    "permission denied",
    "device is not unlocked",
    "read-only file system",
];

/// Information about a remount operation.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct RemountInfo {
    /// Path that was remounted
    pub path: String,
    /// Mode that was used for remounting
    pub mode: String,
}

impl FromStr for RemountInfo {
    type Err = RustADBError;

    fn from_str(s: &str) -> std::result::Result<Self, Self::Err> {
        let caps = REMOUNT_REGEX
            .captures(s)
            .ok_or_else(|| RustADBError::RemountError(s.to_string()))?;

        let (Some(path), Some(mode)) = (caps.name("path"), caps.name("mode")) else {
            return Err(RustADBError::RemountError(s.to_string()));
        };

        Ok(Self {
            path: path.as_str().to_string(),
            mode: mode.as_str().to_string(),
        })
    }
}

impl RemountInfo {
    /// Parse a complete `remount:` reply.
    ///
    /// Succeeds when the reply carries a success marker (`remount succeeded`) or
    /// at least one `Using <path> for <mode>` line, and no failure marker. The
    /// full reply is preserved by the caller's [`RemountOutcome`] for
    /// diagnostics.
    ///
    /// [`RemountOutcome`]: crate::models::RemountOutcome
    pub(crate) fn from_str_response(s: &str) -> Result<Vec<Self>> {
        let lower = s.to_ascii_lowercase();
        if REMOUNT_FAILURE_MARKERS
            .iter()
            .any(|marker| lower.contains(marker))
        {
            return Err(RustADBError::RemountError(s.to_string()));
        }

        let mut infos = Vec::new();
        for line in s.lines() {
            if line.starts_with("Using") {
                infos.push(Self::from_str(line)?);
            }
        }

        if infos.is_empty() && !lower.contains("remount succeeded") {
            return Err(RustADBError::RemountError(s.to_string()));
        }

        Ok(infos)
    }
}

#[cfg(test)]
mod tests {
    use super::RemountInfo;
    use crate::RustADBError;

    #[test]
    fn parses_multiline_success_reply() {
        let reply = "Using overlayfs for /system\nUsing overlayfs for /vendor\nremount succeeded\n";
        let infos = RemountInfo::from_str_response(reply).expect("multi-line reply is a success");
        assert_eq!(infos.len(), 2, "every Using line must be parsed");
        // AOSP renders `Using <mode> for <mount-point>`; the existing field names
        // predate this parse and keep the on-wire order.
        assert_eq!(infos[0].path, "overlayfs");
        assert_eq!(infos[0].mode, "/system");
    }

    #[test]
    fn accepts_success_without_partition_lines() {
        let infos = RemountInfo::from_str_response("remount succeeded\n")
            .expect("a bare success marker is a success");
        assert!(infos.is_empty(), "no Using lines means no partition detail");
    }

    #[test]
    fn accepts_vendor_success_wording_with_using_lines() {
        let infos = RemountInfo::from_str_response("Using ext4 for /system\nremount complete\n")
            .expect("Using lines alone are enough without a failure marker");
        assert_eq!(infos.len(), 1);
    }

    #[test]
    fn rejects_failure_markers() {
        for reply in [
            "remount failed: Permission denied\n",
            "Not running as root\n",
            "remount failed\n",
            "Permission denied\n",
        ] {
            assert!(
                matches!(
                    RemountInfo::from_str_response(reply),
                    Err(RustADBError::RemountError(_))
                ),
                "failure reply must be rejected: {reply:?}"
            );
        }
    }

    #[test]
    fn rejects_empty_or_unrecognized_reply() {
        for reply in ["", "something unexpected\n"] {
            assert!(
                matches!(
                    RemountInfo::from_str_response(reply),
                    Err(RustADBError::RemountError(_))
                ),
                "unrecognized reply must be rejected: {reply:?}"
            );
        }
    }
}
