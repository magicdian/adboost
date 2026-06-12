//! Optional parity check against the official `adb` client.
//!
//! When a real `adb` binary is on `PATH`, we can prove adboost's server frontend
//! behaves like a real adb server *to a real adb client*: point `adb -P <port>`
//! at adboost's in-process server and run a command, comparing the result to the
//! same command run against the user's normal adb server.
//!
//! This is best-effort and entirely auto-detected: if `adb` is absent, or the
//! probe can't run, the whole group is reported SKIPPED — it never blocks the
//! core suite.

use std::net::SocketAddrV4;
use std::process::Stdio;

use tokio::process::Command;

use super::report::Outcome;

/// Whether an `adb` binary is invokable (`adb version` exits cleanly).
pub async fn adb_available() -> bool {
    Command::new("adb")
        .arg("version")
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await
        .map(|s| s.success())
        .unwrap_or(false)
}

/// Run `adb -P <port> shell echo <marker>` against adboost's in-process server
/// and assert the marker round-trips. Proves a real adb client can drive the
/// adboost server frontend.
///
/// `serial` selects the device on the adboost server (`-s`), matching the rest
/// of the harness's multi-device handling.
pub async fn case_official_adb_shell(addr: SocketAddrV4, serial: &str) -> Outcome {
    const MARKER: &str = "adboost_parity_marker_d31f";
    let port = addr.port().to_string();
    let output = Command::new("adb")
        .args(["-P", &port, "-s", serial, "shell", "echo", MARKER])
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(out) if out.status.success() => {
            let stdout = String::from_utf8_lossy(&out.stdout);
            if stdout.trim_end() == MARKER {
                Outcome::Passed
            } else {
                Outcome::Failed(format!(
                    "official adb shell echo returned {:?}, expected {MARKER:?}",
                    stdout.trim_end()
                ))
            }
        }
        Ok(out) => Outcome::Failed(format!(
            "official adb against adboost server exited {}: {}",
            out.status,
            String::from_utf8_lossy(&out.stderr).trim_end()
        )),
        Err(e) => Outcome::Failed(format!("could not invoke adb: {e}")),
    }
}
