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

/// Run `adb -P <port> shell echo <marker>` against adboost's in-process server
/// with **no `-s`** while multiple devices are connected, and assert the client
/// is told the selection is ambiguous (`more than one device`).
///
/// This reproduces the exact reported regression end-to-end: a modern `adb`
/// selects a transport via `host:tport:any` *before* sending `shell:`, and the
/// server frontend used to collapse the multi-device case into the misleading
/// `device not found`. The AOSP-correct reply is `more than one device/emulator`
/// (we assert the stable `more than one device` substring, tolerant of the
/// `/emulator` suffix).
///
/// Only meaningful in the multi-device scenario; the harness runs it once per
/// run (not per serial) and only when `multi` is true.
pub async fn case_official_adb_ambiguous_shell(addr: SocketAddrV4) -> Outcome {
    const MARKER: &str = "adboost_parity_marker_ambiguous";
    let port = addr.port().to_string();
    let output = Command::new("adb")
        .args(["-P", &port, "shell", "echo", MARKER])
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        // A no-`-s` shell against multiple devices must NOT succeed.
        Ok(out) if out.status.success() => Outcome::Failed(
            "official adb shell with no -s unexpectedly succeeded against multiple devices; \
             expected `more than one device`"
                .to_string(),
        ),
        Ok(out) => {
            let stderr = String::from_utf8_lossy(&out.stderr);
            if stderr.contains("more than one device") {
                Outcome::Passed
            } else if stderr.contains("device not found") {
                Outcome::Failed(format!(
                    "REGRESSION: no--s shell against multiple devices reported `device not found` \
                     instead of `more than one device`: {}",
                    stderr.trim_end()
                ))
            } else {
                Outcome::Failed(format!(
                    "no--s shell against multiple devices failed with unexpected wording, \
                     expected `more than one device`: {}",
                    stderr.trim_end()
                ))
            }
        }
        Err(e) => Outcome::Failed(format!("could not invoke adb: {e}")),
    }
}

/// Drive the official `adb` client's `connect` against adboost's in-process
/// server to prove the `host:connect` arm is routed and framed end-to-end —
/// **without** mutating any real device. We connect to a deliberately
/// unreachable loopback port: the server reaches the backend, the TCP dial
/// fails, and `adb` prints a `failed to connect`/`cannot connect` line. This is
/// non-destructive (no USB device is touched), so it is safe in the automated
/// phase.
///
/// The regression it locks against is the originally-reported
/// `unknown host service: connect:` — that means the arm is missing entirely.
/// Any connect-machinery wording (connected / failed to connect / cannot
/// connect / could not resolve) proves the arm is wired.
pub async fn case_official_adb_connect_routing(addr: SocketAddrV4) -> Outcome {
    let port = addr.port().to_string();
    // Port 1 on loopback is unbound in practice → connect is refused fast.
    let bad_target = "127.0.0.1:1";
    let output = Command::new("adb")
        .args(["-P", &port, "connect", bad_target])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if combined.contains("unknown host service") {
                return Outcome::Failed(format!(
                    "REGRESSION: `adb connect` reported `unknown host service` — the \
                     host:connect arm is missing: {}",
                    combined.trim()
                ));
            }
            if combined.contains("connected to")
                || combined.contains("failed to connect")
                || combined.contains("cannot connect")
                || combined.contains("could not resolve")
            {
                Outcome::Passed
            } else {
                Outcome::Failed(format!(
                    "`adb connect` produced unexpected output (no connect-machinery \
                     wording): {}",
                    combined.trim()
                ))
            }
        }
        Err(e) => Outcome::Failed(format!("could not invoke adb: {e}")),
    }
}

/// Drive the official `adb` client's *bare* `get-state` against adboost's
/// in-process server — **no `-s`**, so the client emits the transport-any
/// `host:get-state` (not the serial-pinned `host-serial:<serial>:get-state`)
/// that the frontend resolves against the single connected device.
///
/// This is the runtime guard for the originally-reported regression: AOSP
/// `adb root`/`unroot` call `adb_get_state()` (bare `host:get-state`) before
/// issuing `root:`/`unroot:`, and aborted the whole flow on
/// `unknown host service: get-state`. Any structured reply (a state string like
/// `device`, or an AOSP transport-any `FAIL` wording) proves the arm is wired;
/// `unknown host service` means it is still missing. Like the connect case it
/// is non-destructive, so it is safe in the automated phase.
pub async fn case_official_adb_get_state(addr: SocketAddrV4) -> Outcome {
    let port = addr.port().to_string();
    let output = Command::new("adb")
        .args(["-P", &port, "get-state"])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await;

    match output {
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            if combined.contains("unknown host service") {
                return Outcome::Failed(format!(
                    "REGRESSION: bare `adb get-state` reported `unknown host service` — the \
                     transport-any host:get-state arm is missing: {}",
                    combined.trim()
                ));
            }
            // With a single connected, state `device` device the client prints a
            // success state word; with zero/multiple devices it prints an AOSP
            // transport error we also route correctly. Any non-`unknown-host-service`
            // outcome proves the arm is wired.
            Outcome::Passed
        }
        Err(e) => Outcome::Failed(format!("could not invoke adb: {e}")),
    }
}

/// Run `adb -P <port> -s <tcp_serial> shell echo <marker>` against adboost's
/// in-process server, where `tcp_serial` is a `host:connect`ed TCP/IP device.
/// This is the `PR4b` end-to-end assertion: a client local service (`shell:`)
/// bridged *through* the server to a TCP/IP device (not a USB one).
///
/// The caller is responsible for having already `host:connect`ed the device on
/// the server; here we only drive the shell and check the marker round-trips.
/// Hardware-only (needs a real reachable TCP/IP device), so it lives in the
/// interactive phase.
pub async fn case_official_adb_shell_through_tcp_device(
    addr: SocketAddrV4,
    tcp_serial: &str,
) -> Outcome {
    const MARKER: &str = "adboost_tcpip_through_server_marker";
    let port = addr.port().to_string();
    let output = Command::new("adb")
        .args(["-P", &port, "-s", tcp_serial, "shell", "echo", MARKER])
        .stdout(Stdio::piped())
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
                    "shell through TCP/IP device returned {:?}, expected {MARKER:?}",
                    stdout.trim_end()
                ))
            }
        }
        Ok(out) => {
            let combined = format!(
                "{}{}",
                String::from_utf8_lossy(&out.stdout),
                String::from_utf8_lossy(&out.stderr)
            );
            // The pre-PR4b symptom: the bridge refused TCP serials with a stable
            // "not yet supported" reason. Flag it explicitly as a regression.
            if combined.contains("not yet supported") {
                Outcome::Failed(format!(
                    "REGRESSION: shell through a host:connect'd TCP/IP device reported \
                     `not yet supported` — the transport-generalized multiplexer is not wired: {}",
                    combined.trim()
                ))
            } else {
                Outcome::Failed(format!(
                    "shell through TCP/IP device exited {}: {}",
                    out.status,
                    combined.trim()
                ))
            }
        }
        Err(e) => Outcome::Failed(format!("could not invoke adb: {e}")),
    }
}

/// `adb -P <port> connect <addr>` against adboost's in-process server; returns
/// the combined stdout+stderr so the caller can confirm the device joined.
/// Hardware-driving helper for the interactive tcpip end-to-end case.
pub async fn adb_connect(addr: SocketAddrV4, target: &str) -> Result<String, String> {
    let port = addr.port().to_string();
    let output = Command::new("adb")
        .args(["-P", &port, "connect", target])
        .stdout(Stdio::piped())
        .stderr(Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("could not invoke adb connect: {e}"))?;
    Ok(format!(
        "{}{}",
        String::from_utf8_lossy(&output.stdout),
        String::from_utf8_lossy(&output.stderr)
    ))
}

/// `adb -P <port> disconnect <addr>` against adboost's in-process server.
/// Best-effort teardown for the interactive tcpip case.
pub async fn adb_disconnect(addr: SocketAddrV4, target: &str) {
    let port = addr.port().to_string();
    let _ = Command::new("adb")
        .args(["-P", &port, "disconnect", target])
        .stdout(Stdio::null())
        .stderr(Stdio::null())
        .status()
        .await;
}
