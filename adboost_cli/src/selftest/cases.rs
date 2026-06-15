//! Device-exercising test cases, generic over any [`ADBDeviceExt`].
//!
//! Each `case_*` function performs one self-contained check against a device and
//! returns an [`Outcome`]. They are deliberately generic so the SAME logic runs
//! against both channels — a USB-direct [`ADBUSBDevice`] and a
//! through-the-server [`ADBProxyDevice`] — proving parity between adboost's
//! direct path and its server frontend.
//!
//! Cases avoid leaving persistent device state: files are written under
//! `/data/local/tmp` with unique names and removed at the end.

use adb_client::ADBDeviceExt;
use adb_client::usb::PersistentUsbConnection;

use super::report::Outcome;

/// A scratch directory writable without root on essentially every device.
const SCRATCH_DIR: &str = "/data/local/tmp";

/// Marker echoed by the shell round-trip cases.
const ECHO_MARKER: &str = "adboost_selftest_marker_4f2a";

// ---------------------------------------------------------------------------
// Persistent-connection cases (USB-direct channel).
//
// The USB-direct channel runs against a `PersistentUsbConnection` (not the
// non-persistent `ADBUSBDevice`): it multiplexes many sessions over ONE
// authenticated connection and sends a connection-level CLSE on drop, so it
// handles several sequential services cleanly — exactly the same primitive the
// server backend uses. These cases drive its public ops directly.
// ---------------------------------------------------------------------------

/// `shell echo` over the persistent connection round-trips a marker.
pub async fn persistent_shell_echo(conn: &PersistentUsbConnection) -> Outcome {
    match conn.shell_exec(&format!("echo {ECHO_MARKER}")).await {
        Ok((out, _)) if out.trim_end() == ECHO_MARKER => Outcome::Passed,
        Ok((out, _)) => Outcome::Failed(format!("echo returned {out:?}, expected {ECHO_MARKER:?}")),
        Err(e) => Outcome::Failed(format!("shell_exec failed: {e}")),
    }
}

/// `shell,v2 echo` over the persistent connection: separated stdout + a real
/// exit code (proves the shell-v2 inner framing decodes).
pub async fn persistent_shell_v2(conn: &PersistentUsbConnection) -> Outcome {
    let mut session = match conn.open_shell_v2(&format!("echo {ECHO_MARKER}")).await {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(format!("open_shell_v2 failed: {e}")),
    };
    match session.execute().await {
        Ok(out) => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.trim_end() != ECHO_MARKER {
                return Outcome::Failed(format!(
                    "shell_v2 stdout {stdout:?}, expected {ECHO_MARKER:?}"
                ));
            }
            match out.exit_code {
                Some(0) => Outcome::Passed,
                Some(c) => Outcome::Failed(format!("shell_v2 echo exited {c}, expected 0")),
                None => Outcome::Failed("shell_v2 reported no exit code".into()),
            }
        }
        Err(e) => Outcome::Failed(format!("shell_v2 execute failed: {e}")),
    }
}

/// SYNC push→pull round-trip over the persistent connection's sync session.
pub async fn persistent_push_pull(conn: &PersistentUsbConnection) -> Outcome {
    let payload = b"adboost selftest payload \x00\x01\x02 end".to_vec();
    let remote = format!("{SCRATCH_DIR}/adboost_selftest_pers_8b1c.bin");

    let mut sync = match conn.open_sync_session().await {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(format!("open_sync_session (push) failed: {e}")),
    };
    let reader: &[u8] = &payload;
    if let Err(e) = sync.push(reader, &remote, 0o644).await {
        return Outcome::Failed(format!("sync push failed: {e}"));
    }
    drop(sync);

    // A fresh sync session for the pull (SYNC is one transaction per session).
    let mut sync = match conn.open_sync_session().await {
        Ok(s) => s,
        Err(e) => return Outcome::Failed(format!("open_sync_session (pull) failed: {e}")),
    };
    let mut pulled = Vec::new();
    let pull_res = sync.pull(&remote, &mut pulled).await;
    drop(sync);

    // Best-effort cleanup.
    let _ = conn.shell_exec(&format!("rm -f {remote}")).await;

    match pull_res {
        Ok(()) if pulled == payload => Outcome::Passed,
        Ok(()) => Outcome::Failed(format!(
            "pulled {} bytes differ from pushed {}",
            pulled.len(),
            payload.len()
        )),
        Err(e) => Outcome::Failed(format!("sync pull failed: {e}")),
    }
}

/// Run `cmd` and capture stdout as a lossy UTF-8 string plus the exit code.
async fn run_shell<D: ADBDeviceExt>(
    device: &mut D,
    cmd: &str,
) -> Result<(String, Option<u8>), String> {
    let mut stdout = Vec::new();
    let code = device
        .shell_command(&cmd, Some(&mut stdout), None)
        .await
        .map_err(|e| format!("shell `{cmd}` failed: {e}"))?;
    Ok((String::from_utf8_lossy(&stdout).into_owned(), code))
}

/// `shell echo` round-trips a known marker back to the host.
pub async fn case_shell_echo<D: ADBDeviceExt>(device: &mut D) -> Outcome {
    match run_shell(device, &format!("echo {ECHO_MARKER}")).await {
        Ok((out, _)) if out.trim_end() == ECHO_MARKER => Outcome::Passed,
        Ok((out, _)) => Outcome::Failed(format!("echo returned {out:?}, expected {ECHO_MARKER:?}")),
        Err(e) => Outcome::Failed(e),
    }
}

/// `shell true` / `shell false` report exit codes (when the channel surfaces
/// them). A channel that always returns `None` (v1 with no exit-code support)
/// is not penalized — the case only fails on a *wrong* non-null code.
pub async fn case_shell_exit_code<D: ADBDeviceExt>(device: &mut D) -> Outcome {
    let (_, false_code) = match run_shell(device, "false").await {
        Ok(v) => v,
        Err(e) => return Outcome::Failed(e),
    };
    match false_code {
        None => Outcome::Skipped("channel does not surface shell exit codes".into()),
        Some(0) => Outcome::Failed("`false` reported exit code 0".into()),
        Some(_) => Outcome::Passed,
    }
}

/// `push` a payload then `pull` it back and compare bytes — the SYNC round-trip.
pub async fn case_push_pull_roundtrip<D: ADBDeviceExt>(device: &mut D) -> Outcome {
    let payload = b"adboost selftest payload \x00\x01\x02 end".to_vec();
    let remote = format!("{SCRATCH_DIR}/adboost_selftest_pushpull_8b1c.bin");

    // Push. `&[u8]` implements `AsyncRead` (the slice acts as a cursor).
    let mut reader: &[u8] = &payload;
    if let Err(e) = device.push(&mut reader, &remote).await {
        return Outcome::Failed(format!("push failed: {e}"));
    }

    // Pull back into memory.
    let mut pulled = Vec::new();
    let pull_res = device.pull(&remote, &mut pulled).await;

    // Best-effort cleanup regardless of pull outcome.
    let _ = run_shell(device, &format!("rm -f {remote}")).await;

    match pull_res {
        Ok(()) if pulled == payload => Outcome::Passed,
        Ok(()) => Outcome::Failed(format!(
            "pulled {} bytes differ from pushed {} bytes",
            pulled.len(),
            payload.len()
        )),
        Err(e) => Outcome::Failed(format!("pull failed: {e}")),
    }
}

/// `list` the scratch dir — exercises the SYNC LIST path and basic dir reads.
pub async fn case_list_scratch_dir<D: ADBDeviceExt>(device: &mut D) -> Outcome {
    match device.list(&SCRATCH_DIR).await {
        Ok(_) => Outcome::Passed,
        Err(e) => Outcome::Failed(format!("list `{SCRATCH_DIR}` failed: {e}")),
    }
}

/// `stat` a path that always exists — exercises the STAT path.
pub async fn case_stat_root<D: ADBDeviceExt>(device: &mut D) -> Outcome {
    match device.stat(&SCRATCH_DIR).await {
        Ok(_) => Outcome::Passed,
        Err(e) => Outcome::Failed(format!("stat `{SCRATCH_DIR}` failed: {e}")),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use adb_client::{ADBListItemType, AdbStatResponse, RustADBError};
    use std::pin::Pin;
    use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite, AsyncWriteExt};

    /// Which `ADBDeviceExt` method the fake should fail (one at a time keeps the
    /// fake's behavior unambiguous and avoids a pile of boolean flags).
    #[derive(Default, PartialEq, Eq)]
    enum FailMode {
        #[default]
        None,
        Shell,
        Push,
        Pull,
        List,
    }

    /// A scriptable fake device implementing just enough of `ADBDeviceExt` to
    /// exercise the case logic without hardware.
    #[derive(Default)]
    struct FakeDevice {
        /// stdout to emit for the next `shell_command`, and the exit code.
        shell_stdout: Vec<u8>,
        shell_code: Option<u8>,
        /// Storage for the push/pull round-trip (pull echoes what push stored).
        pushed: Vec<u8>,
        /// Which method, if any, should return an error.
        fail: FailMode,
    }

    impl ADBDeviceExt for FakeDevice {
        async fn shell_command(
            &mut self,
            _command: &(dyn AsRef<str> + Sync),
            stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
            _stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        ) -> Result<Option<u8>, RustADBError> {
            if self.fail == FailMode::Shell {
                return Err(RustADBError::ADBRequestFailed("shell boom".into()));
            }
            if let Some(out) = stdout {
                out.write_all(&self.shell_stdout).await?;
            }
            Ok(self.shell_code)
        }

        async fn shell(
            &mut self,
            _reader: &mut (dyn AsyncRead + Unpin + Send),
            _writer: Pin<Box<dyn AsyncWrite + Send>>,
        ) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn exec(
            &mut self,
            _command: &str,
            _reader: &mut (dyn AsyncRead + Unpin + Send),
            _writer: Pin<Box<dyn AsyncWrite + Send>>,
        ) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn stat(
            &mut self,
            _remote_path: &(dyn AsRef<str> + Sync),
        ) -> Result<AdbStatResponse, RustADBError> {
            Ok(AdbStatResponse {
                file_perm: 0o040_755,
                file_size: 0,
                mod_time: 0,
            })
        }

        async fn pull(
            &mut self,
            _source: &(dyn AsRef<str> + Sync),
            output: &mut (dyn AsyncWrite + Unpin + Send),
        ) -> Result<(), RustADBError> {
            if self.fail == FailMode::Pull {
                return Err(RustADBError::ADBRequestFailed("pull boom".into()));
            }
            output.write_all(&self.pushed).await?;
            Ok(())
        }

        async fn push(
            &mut self,
            stream: &mut (dyn AsyncRead + Unpin + Send),
            _path: &(dyn AsRef<str> + Sync),
        ) -> Result<(), RustADBError> {
            if self.fail == FailMode::Push {
                return Err(RustADBError::ADBRequestFailed("push boom".into()));
            }
            let mut buf = Vec::new();
            stream.read_to_end(&mut buf).await?;
            self.pushed = buf;
            Ok(())
        }

        async fn list(
            &mut self,
            _path: &(dyn AsRef<str> + Sync),
        ) -> Result<Vec<ADBListItemType>, RustADBError> {
            if self.fail == FailMode::List {
                return Err(RustADBError::ADBRequestFailed("list boom".into()));
            }
            Ok(vec![])
        }

        async fn reboot(
            &mut self,
            _reboot_type: adb_client::RebootType,
        ) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn remount(&mut self) -> Result<Vec<adb_client::RemountInfo>, RustADBError> {
            Ok(vec![])
        }

        async fn root(&mut self) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn install(
            &mut self,
            _apk_path: &(dyn AsRef<std::path::Path> + Sync),
            _user: Option<&str>,
        ) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn uninstall(
            &mut self,
            _package: &(dyn AsRef<str> + Sync),
            _user: Option<&str>,
        ) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn enable_verity(&mut self) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn disable_verity(&mut self) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn tcpip(&mut self, port: u16) -> Result<String, RustADBError> {
            Ok(format!("restarting in TCP mode port: {port}"))
        }

        async fn usb(&mut self) -> Result<(), RustADBError> {
            Ok(())
        }

        async fn framebuffer_inner(
            &mut self,
        ) -> Result<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>, RustADBError> {
            Ok(image::ImageBuffer::new(1, 1))
        }
    }

    #[tokio::test]
    async fn shell_echo_passes_on_marker() {
        let mut d = FakeDevice {
            shell_stdout: b"adboost_selftest_marker_4f2a\n".to_vec(),
            ..Default::default()
        };
        assert_eq!(case_shell_echo(&mut d).await, Outcome::Passed);
    }

    #[tokio::test]
    async fn shell_echo_fails_on_wrong_output() {
        let mut d = FakeDevice {
            shell_stdout: b"nope\n".to_vec(),
            ..Default::default()
        };
        assert!(matches!(case_shell_echo(&mut d).await, Outcome::Failed(_)));
    }

    #[tokio::test]
    async fn shell_exit_code_skips_when_none() {
        let mut d = FakeDevice {
            shell_code: None,
            ..Default::default()
        };
        assert!(matches!(
            case_shell_exit_code(&mut d).await,
            Outcome::Skipped(_)
        ));
    }

    #[tokio::test]
    async fn shell_exit_code_fails_when_false_returns_zero() {
        let mut d = FakeDevice {
            shell_code: Some(0),
            ..Default::default()
        };
        assert!(matches!(
            case_shell_exit_code(&mut d).await,
            Outcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn shell_exit_code_passes_on_nonzero() {
        let mut d = FakeDevice {
            shell_code: Some(1),
            ..Default::default()
        };
        assert_eq!(case_shell_exit_code(&mut d).await, Outcome::Passed);
    }

    #[tokio::test]
    async fn push_pull_roundtrip_passes_when_bytes_match() {
        // FakeDevice echoes pushed bytes back on pull → round-trip matches.
        let mut d = FakeDevice::default();
        assert_eq!(case_push_pull_roundtrip(&mut d).await, Outcome::Passed);
    }

    #[tokio::test]
    async fn push_pull_roundtrip_fails_on_push_error() {
        let mut d = FakeDevice {
            fail: FailMode::Push,
            ..Default::default()
        };
        assert!(matches!(
            case_push_pull_roundtrip(&mut d).await,
            Outcome::Failed(_)
        ));
    }

    #[tokio::test]
    async fn list_and_stat_pass_on_ok() {
        let mut d = FakeDevice::default();
        assert_eq!(case_list_scratch_dir(&mut d).await, Outcome::Passed);
        assert_eq!(case_stat_root(&mut d).await, Outcome::Passed);
    }

    #[tokio::test]
    async fn list_fails_on_error() {
        let mut d = FakeDevice {
            fail: FailMode::List,
            ..Default::default()
        };
        assert!(matches!(
            case_list_scratch_dir(&mut d).await,
            Outcome::Failed(_)
        ));
    }
}
