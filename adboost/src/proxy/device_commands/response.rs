//! Shared reply handling for device control services (`install`, `uninstall`,
//! `root`, `unroot`, `remount`).
//!
//! Every one of these services answers `OKAY`, streams a short human-readable
//! text, and closes the socket. The old proxy code read that text **once** into
//! a fixed 1024-byte buffer and compared it byte-for-byte, which misclassified
//! fragmented replies and truncated multi-line output. This module owns the
//! replacement: one capped, optionally cancellable read plus pure classifiers,
//! so the reply grammar lives in exactly one place.

use tokio::io::{AsyncRead, AsyncReadExt};
use tokio_util::sync::CancellationToken;

use crate::models::RootState;
use crate::{Result, RustADBError};

/// Hard ceiling for a control-service textual reply.
///
/// Far above any real `install`/`uninstall`/`root`/`remount` output; the cap
/// exists so a misbehaving device cannot make the proxy grow memory without
/// bound.
pub(crate) const MAX_CONTROL_REPLY_BYTES: usize = 64 * 1024;

const READ_CHUNK_SIZE: usize = 4096;

/// Read a control-service reply to EOF, bounded by [`MAX_CONTROL_REPLY_BYTES`].
///
/// This replaces the single fixed-size read used before: replies may arrive in
/// any number of fragments and are accumulated until the device closes the
/// socket (the normal terminator for these one-shot services).
///
/// When `cancel` is supplied, each read races the token's `cancelled()` future
/// (both are cancel-safe) and a triggered token yields
/// [`RustADBError::OperationCancelled`]. The caller is responsible for closing
/// the connection on cancellation.
pub(crate) async fn read_reply<R>(
    reader: &mut R,
    cancel: Option<&CancellationToken>,
) -> Result<String>
where
    R: AsyncRead + Unpin,
{
    let mut body = Vec::new();
    let mut chunk = [0u8; READ_CHUNK_SIZE];

    loop {
        let remaining = MAX_CONTROL_REPLY_BYTES.saturating_sub(body.len());
        if remaining == 0 {
            tracing::warn!(
                "control reply exceeded {MAX_CONTROL_REPLY_BYTES} bytes; truncating (the \
                 connection will be reset)"
            );
            break;
        }
        let capacity = remaining.min(chunk.len());

        let read = if let Some(cancel) = cancel {
            tokio::select! {
                biased;
                () = cancel.cancelled() => return Err(RustADBError::OperationCancelled),
                result = reader.read(&mut chunk[..capacity]) => result?,
            }
        } else {
            reader.read(&mut chunk[..capacity]).await?
        };

        if read == 0 {
            break;
        }
        body.extend_from_slice(&chunk[..read]);
    }

    Ok(String::from_utf8_lossy(&body).into_owned())
}

/// One classification of a package-manager success/failure reply.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(crate) enum ReplyKind {
    /// The device reported `Success`.
    Success,
    /// The device reported `Failure [REASON]`; carries the extracted reason.
    Failure(String),
    /// Unrecognized reply; the raw text is the caller's evidence.
    Unknown,
}

/// Classify a `cmd package install` / `cmd package uninstall` reply.
///
/// Device builds may prefix the verdict with warning noise (`avc: …`), so the
/// last decisive `Success`/`Failure` line wins rather than the whole body being
/// compared against one exact string.
pub(crate) fn parse_success_or_failure(raw: &str) -> ReplyKind {
    let mut verdict = ReplyKind::Unknown;

    for line in raw.lines() {
        let line = line.trim();
        if line.is_empty() {
            continue;
        }
        if let Some(rest) = line.strip_prefix("Success") {
            // Accept `Success` alone plus any trailing detail after whitespace or
            // a colon; never match an unrelated word that merely starts the same.
            if rest.is_empty() || rest.starts_with(char::is_whitespace) || rest.starts_with(':') {
                verdict = ReplyKind::Success;
            }
        } else if let Some(rest) = line.strip_prefix("Failure") {
            let reason = extract_failure_reason(rest);
            verdict = ReplyKind::Failure(if reason.is_empty() {
                "unknown response".to_string()
            } else {
                reason
            });
        }
    }

    verdict
}

/// Extract `REASON` from the tail of a `Failure [REASON]` line.
fn extract_failure_reason(rest: &str) -> String {
    let rest = rest.trim();
    rest.strip_prefix('[')
        .and_then(|inner| inner.strip_suffix(']'))
        .unwrap_or(rest)
        .trim()
        .to_string()
}

/// Which privilege direction a receipt belongs to.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum RootDirection {
    /// `adb root`
    Root,
    /// `adb unroot`
    Unroot,
}

/// Classify an `adb root` / `adb unroot` receipt.
///
/// adbd answers with one stable AOSP line; unknown wording is preserved as
/// [`RootState::Unknown`] rather than assumed to be success.
pub(crate) fn classify_root_receipt(raw: &str, direction: RootDirection) -> RootState {
    let text = raw.trim();
    if text.is_empty() {
        return RootState::Unknown;
    }
    let lower = text.to_ascii_lowercase();

    // Refusal is the one must-not-ignore answer: never let it fall through to a
    // success-looking classification.
    if lower.contains("cannot run as root") {
        return RootState::Refused;
    }

    match direction {
        RootDirection::Root => {
            if lower.contains("already running as root") {
                RootState::AlreadyRoot
            } else if lower.contains("restarting adbd as root") {
                RootState::Restarting
            } else {
                RootState::Unknown
            }
        }
        RootDirection::Unroot => {
            if lower.contains("not running as root")
                || lower.contains("already running as non root")
                || lower.contains("already running as nonroot")
            {
                RootState::AlreadyRoot
            } else if lower.contains("restarting adbd as non root")
                || lower.contains("restarting adbd as nonroot")
            {
                RootState::Restarting
            } else {
                RootState::Unknown
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::{
        MAX_CONTROL_REPLY_BYTES, ReplyKind, RootDirection, classify_root_receipt,
        parse_success_or_failure, read_reply,
    };
    use crate::RustADBError;
    use crate::models::RootState;
    use tokio::io::{AsyncWriteExt, duplex};
    use tokio_util::sync::CancellationToken;

    #[tokio::test]
    async fn read_reply_accumulates_fragmented_reply() {
        let (mut writer, mut reader) = duplex(64);
        writer
            .write_all(b"Succ")
            .await
            .expect("duplex write cannot fail");
        writer
            .write_all(b"ess\n")
            .await
            .expect("duplex write cannot fail");
        drop(writer);

        let raw = read_reply(&mut reader, None)
            .await
            .expect("fragmented reply must be read to EOF");
        assert_eq!(
            raw, "Success\n",
            "a reply split across reads must not be misclassified"
        );
    }

    #[tokio::test]
    async fn read_reply_stops_at_cap() {
        let (mut writer, mut reader) = duplex(MAX_CONTROL_REPLY_BYTES + 1024);
        let payload = vec![b'x'; MAX_CONTROL_REPLY_BYTES + 1024];
        let write = tokio::spawn(async move {
            // Ignore a write error if the reader stops at the cap and closes.
            let _ = writer.write_all(&payload).await;
        });

        let raw = read_reply(&mut reader, None)
            .await
            .expect("the capped read must succeed");
        assert_eq!(
            raw.len(),
            MAX_CONTROL_REPLY_BYTES,
            "the reply must be truncated at the documented ceiling"
        );
        write.await.expect("writer task must finish");
    }

    #[tokio::test]
    async fn read_reply_cancels() {
        let (mut writer, mut reader) = duplex(64);
        writer
            .write_all(b"partial")
            .await
            .expect("duplex write cannot fail");
        let token = CancellationToken::new();
        let cancel = token.clone();

        let cancel_task = tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(20)).await;
            cancel.cancel();
        });

        let error = read_reply(&mut reader, Some(&token))
            .await
            .expect_err("a blocked read must observe cancellation");
        assert!(
            matches!(error, RustADBError::OperationCancelled),
            "cancellation must surface as OperationCancelled: {error:?}"
        );
        cancel_task.await.expect("cancel task must finish");
    }

    #[test]
    fn parses_success_reply() {
        assert_eq!(parse_success_or_failure("Success\n"), ReplyKind::Success);
        assert_eq!(
            parse_success_or_failure("Success"),
            ReplyKind::Success,
            "a missing trailing newline is still success"
        );
    }

    #[test]
    fn parses_failure_reason() {
        assert_eq!(
            parse_success_or_failure("Failure [INSTALL_FAILED_INSUFFICIENT_STORAGE]\n"),
            ReplyKind::Failure("INSTALL_FAILED_INSUFFICIENT_STORAGE".to_string())
        );
        assert_eq!(
            parse_success_or_failure("Failure [DELETE_FAILED_INTERNAL_ERROR]"),
            ReplyKind::Failure("DELETE_FAILED_INTERNAL_ERROR".to_string())
        );
    }

    #[test]
    fn verdict_survives_leading_noise() {
        assert_eq!(
            parse_success_or_failure("avc: denied { read } for pid=1\nSuccess\n"),
            ReplyKind::Success,
            "warning lines before the verdict must not hide it"
        );
    }

    #[test]
    fn unknown_reply_is_reported_as_unknown() {
        for reply in ["", "something else\n", "Failure\n"] {
            let kind = parse_success_or_failure(reply);
            assert!(
                matches!(&kind, ReplyKind::Unknown)
                    || matches!(&kind, ReplyKind::Failure(reason) if reason == "unknown response"),
                "unrecognized reply must not look like success: {reply:?} => {kind:?}"
            );
        }
    }

    #[test]
    fn classifies_root_receipts() {
        let cases = [
            ("restarting adbd as root\n", RootState::Restarting),
            ("adbd is already running as root\n", RootState::AlreadyRoot),
            (
                "adbd cannot run as root in production builds\n",
                RootState::Refused,
            ),
            ("", RootState::Unknown),
            ("unexpected vendor text\n", RootState::Unknown),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                classify_root_receipt(raw, RootDirection::Root),
                expected,
                "root receipt {raw:?} must classify as {expected:?}"
            );
        }
    }

    #[test]
    fn classifies_unroot_receipts() {
        let cases = [
            ("restarting adbd as non root\n", RootState::Restarting),
            ("adbd not running as root\n", RootState::AlreadyRoot),
            (
                "adbd is already running as non root\n",
                RootState::AlreadyRoot,
            ),
            ("unknown\n", RootState::Unknown),
        ];
        for (raw, expected) in cases {
            assert_eq!(
                classify_root_receipt(raw, RootDirection::Unroot),
                expected,
                "unroot receipt {raw:?} must classify as {expected:?}"
            );
        }
    }

    #[test]
    fn refusal_beats_every_other_marker() {
        assert_eq!(
            classify_root_receipt(
                "adbd cannot run as root in production builds",
                RootDirection::Unroot
            ),
            RootState::Refused,
            "a refusal must never be downgraded to an unknown/success state"
        );
    }
}
