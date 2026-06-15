//! Interactive (human-in-the-loop) self-test phase.
//!
//! These cases require a person to physically act on the device (unplug/replug
//! USB) or wait out a reboot. They run after the automated phase, each gated
//! behind a prompt so an operator opts in per case. Device presence is polled
//! via USB enumeration; a recovered device is re-validated with a shell echo.
//!
//! Timeouts bound every wait so the harness never hangs: device-return waits use
//! [`RECONNECT_TIMEOUT`]. The reboot case explicitly EXCLUDES tcpip devices (a
//! tcpip connection legitimately needs to be re-established after reboot, which
//! is a different scenario — see the PRD).

use std::time::{Duration, Instant};

use tokio::time::sleep;

use super::channels::{DiscoveredDevice, InProcessServer, discover_devices};
use super::report::{CaseResult, Outcome, Reporter};
use super::{cases, parity};
use adb_client::RebootType;
use adb_client::usb::PersistentUsbConnection;

/// Port adbd listens on after `tcpip <port>` for the interactive end-to-end case.
const TCPIP_PORT: u16 = 5555;

/// How long to wait for a device to disappear / reappear before failing.
const RECONNECT_TIMEOUT: Duration = Duration::from_secs(120);

/// How long to wait for a rebooting device to come back before failing.
const REBOOT_TIMEOUT: Duration = Duration::from_secs(120);

/// Poll interval while waiting on a device-presence change.
const POLL_INTERVAL: Duration = Duration::from_secs(1);

/// Run the interactive phase against the addressable devices.
pub async fn run_interactive_phase(reporter: &mut Reporter, devices: &[&DiscoveredDevice]) {
    println!();
    println!("=== Interactive phase ===");
    if !prompt_yes_no("Run interactive tests (USB re-plug, reboot recovery)?") {
        record(
            reporter,
            "interactive",
            "usb_replug",
            Outcome::Skipped("operator declined interactive phase".into()),
        );
        record(
            reporter,
            "interactive",
            "reboot_recovery",
            Outcome::Skipped("operator declined interactive phase".into()),
        );
        return;
    }

    // Use the first addressable device as the subject of interactive cases.
    let Some(subject) = devices.first().and_then(|d| d.serial.clone()) else {
        record(
            reporter,
            "interactive",
            "usb_replug",
            Outcome::Skipped("no addressable device for interactive cases".into()),
        );
        return;
    };

    let replug = case_usb_replug(&subject).await;
    record(reporter, "interactive", "usb_replug", replug);

    let reboot = case_reboot_recovery(&subject).await;
    record(reporter, "interactive", "reboot_recovery", reboot);

    let tcpip = case_tcpip_through_server(&subject).await;
    record(reporter, "tcpip", "shell_through_tcp_device", tcpip);
}

/// End-to-end tcpip closed loop (`PR4b`): switch a USB device to TCP/IP mode,
/// `host:connect` it through an in-process adboost server, shell to it THROUGH
/// the server (the bridge that `PR4b` enables), then switch the device back to
/// USB to restore its original state.
///
/// Hardware-only and operator-gated; SKIPs cleanly whenever a prerequisite is
/// missing (tcpip subject, no `adb` on PATH, no device IP, operator declines).
/// The non-destructive `host:connect` routing and the device-side `tcpip:`/`usb:`
/// wire encoding are already covered automatically (the connect parity case and
/// the library unit tests); this case is the one piece that needs a live device.
async fn case_tcpip_through_server(serial: &str) -> Outcome {
    if is_tcpip_serial(serial) {
        return Outcome::Skipped("subject is already a tcpip device".into());
    }
    if !parity::adb_available().await {
        return Outcome::Skipped("official `adb` binary not found on PATH".into());
    }
    if !prompt_yes_no(&format!(
        "[tcpip] Switch device {serial} to TCP/IP mode, shell to it through the adboost \
         server, then switch it back to USB? (reversible)"
    )) {
        return Outcome::Skipped("operator declined tcpip end-to-end".into());
    }

    // Discover the device's IP BEFORE switching to tcpip (the shell channel is
    // torn down by the mode switch). Then flip the device to TCP/IP mode.
    let conn = match PersistentUsbConnection::new_from_serial(serial, None).await {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("cannot open device: {e}")),
    };
    let ip = match device_ip(&conn).await {
        Ok(ip) => ip,
        Err(e) => {
            conn.close().await;
            return Outcome::Skipped(format!("could not determine device IP: {e}"));
        }
    };
    // Switch adbd to TCP/IP mode via the dedicated `tcpip:` control service
    // (request-only: `open_session` confirms the device's OKAY, then adbd
    // restarts and drops the connection — like `reboot:`).
    if let Err(e) = conn
        .open_session(&adb_client::ADBLocalCommand::TcpIp(TCPIP_PORT))
        .await
    {
        conn.close().await;
        return Outcome::Failed(format!("tcpip {TCPIP_PORT} failed: {e}"));
    }
    // `tcpip:` restarts adbd and drops the USB connection; close our handle.
    conn.close().await;
    // Give adbd a moment to come back up listening on the TCP port.
    sleep(Duration::from_secs(2)).await;

    let tcp_serial = format!("{ip}:{TCPIP_PORT}");
    let outcome = run_tcpip_through_server(&tcp_serial).await;

    // Restore the device to USB mode regardless of the outcome above. Reconnect
    // over the now-listening TCP transport and issue `usb:`.
    restore_usb_mode(&tcp_serial).await;
    outcome
}

/// Stand up an in-process server, `host:connect` the TCP device, shell to it
/// through the server, and tear the registration down.
async fn run_tcpip_through_server(tcp_serial: &str) -> Outcome {
    let server = match InProcessServer::start().await {
        Ok(s) => s,
        Err(e) => return Outcome::Skipped(format!("cannot start in-process server: {e}")),
    };
    let connect_out = match parity::adb_connect(server.addr(), tcp_serial).await {
        Ok(out) => out,
        Err(e) => {
            server.shutdown().await;
            return Outcome::Failed(e);
        }
    };
    if !(connect_out.contains("connected to") || connect_out.contains("already connected")) {
        parity::adb_disconnect(server.addr(), tcp_serial).await;
        server.shutdown().await;
        return Outcome::Failed(format!(
            "host:connect to {tcp_serial} did not join the device: {}",
            connect_out.trim()
        ));
    }
    let outcome =
        parity::case_official_adb_shell_through_tcp_device(server.addr(), tcp_serial).await;
    parity::adb_disconnect(server.addr(), tcp_serial).await;
    server.shutdown().await;
    outcome
}

/// Best-effort: reconnect to the device over TCP and switch it back to USB mode.
async fn restore_usb_mode(tcp_serial: &str) {
    let Ok(addr) = tcp_serial.parse::<std::net::SocketAddr>() else {
        tracing::warn!("[tcpip] cannot parse {tcp_serial} to restore USB mode");
        return;
    };
    match adb_client::tcp::ADBTcpDevice::new(addr).await {
        Ok(mut dev) => {
            use adb_client::ADBDeviceExt as _;
            if let Err(e) = dev.usb().await {
                tracing::warn!("[tcpip] failed to switch {tcp_serial} back to USB: {e}");
            }
        }
        Err(e) => tracing::warn!("[tcpip] could not reconnect to {tcp_serial} to restore USB: {e}"),
    }
}

/// Read the device's primary IPv4 address via `ip route`, on an existing
/// persistent USB connection. Returns the first `src <ip>` token.
async fn device_ip(conn: &PersistentUsbConnection) -> Result<String, String> {
    let (out, _) = conn
        .shell_exec("ip route")
        .await
        .map_err(|e| format!("shell `ip route` failed: {e}"))?;
    out.split_whitespace()
        .skip_while(|t| *t != "src")
        .nth(1)
        .map(ToOwned::to_owned)
        .ok_or_else(|| format!("no `src <ip>` in: {}", out.trim()))
}

/// USB re-plug: confirm the device disappears when unplugged and reappears
/// (and can shell again) when re-plugged.
async fn case_usb_replug(serial: &str) -> Outcome {
    println!();
    println!("[usb_replug] Please UNPLUG the device with serial {serial} now.");
    if let Err(e) = wait_for_absence(serial, RECONNECT_TIMEOUT).await {
        return Outcome::Failed(e);
    }
    println!("[usb_replug] Detected removal. Now RE-PLUG the device.");
    if let Err(e) = wait_for_presence(serial, RECONNECT_TIMEOUT).await {
        return Outcome::Failed(e);
    }
    // Re-validate: open a fresh USB-direct session and echo.
    verify_shell_after_recovery(serial).await
}

/// Reboot recovery: reboot the device and confirm it returns within the timeout
/// and can shell again. Excludes tcpip devices (their post-reboot reconnect is a
/// separate scenario).
async fn case_reboot_recovery(serial: &str) -> Outcome {
    if is_tcpip_serial(serial) {
        return Outcome::Skipped(
            "reboot-recovery excludes tcpip devices (post-reboot reconnect is a separate scenario)"
                .into(),
        );
    }
    if !prompt_yes_no(&format!(
        "[reboot_recovery] Reboot device {serial}? It will be rebooted now if you agree."
    )) {
        return Outcome::Skipped("operator declined reboot".into());
    }

    // Issue the reboot over the dedicated `reboot:` local service on a fresh
    // persistent connection. NOT `shell_exec("reboot")`: the reboot tears the
    // stream down immediately, which a shell read surfaces as a "session
    // channel closed" error; the `reboot:` service is request-only (OKAY ⇒ done).
    match PersistentUsbConnection::new_from_serial(serial, None).await {
        Ok(conn) => {
            if let Err(e) = conn.reboot(RebootType::System).await {
                return Outcome::Failed(format!("reboot command failed: {e}"));
            }
            conn.close().await;
        }
        Err(e) => return Outcome::Failed(format!("cannot open device to reboot: {e}")),
    }

    println!(
        "[reboot_recovery] Rebooting; waiting up to {}s for the device to return…",
        REBOOT_TIMEOUT.as_secs()
    );
    // The device first disappears, then comes back. Wait for it to go away
    // (best-effort, short window) then for it to return.
    let _ = wait_for_absence(serial, Duration::from_secs(30)).await;
    if let Err(e) = wait_for_presence(serial, REBOOT_TIMEOUT).await {
        return Outcome::Failed(format!("device did not return after reboot: {e}"));
    }
    verify_shell_after_recovery(serial).await
}

/// After a device returns, open a persistent connection and run the shell-echo
/// case to prove the connection is usable again.
async fn verify_shell_after_recovery(serial: &str) -> Outcome {
    // The device may need a moment after re-enumeration before adbd is ready;
    // retry opening a few times within a short budget.
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match PersistentUsbConnection::new_from_serial(serial, None).await {
            Ok(conn) => {
                let outcome = cases::persistent_shell_echo(&conn).await;
                conn.close().await;
                return outcome;
            }
            Err(e) if Instant::now() >= deadline => {
                return Outcome::Failed(format!("device returned but shell not ready: {e}"));
            }
            Err(_) => sleep(POLL_INTERVAL).await,
        }
    }
}

/// Wait until `serial` is no longer enumerated, or time out.
async fn wait_for_absence(serial: &str, timeout: Duration) -> Result<(), String> {
    wait_until(timeout, || !serial_present(serial))
        .await
        .map_err(|()| {
            format!(
                "device {serial} still present after {}s (expected removal)",
                timeout.as_secs()
            )
        })
}

/// Wait until `serial` is enumerated again, or time out.
async fn wait_for_presence(serial: &str, timeout: Duration) -> Result<(), String> {
    wait_until(timeout, || serial_present(serial))
        .await
        .map_err(|()| {
            format!(
                "device {serial} did not reappear within {}s",
                timeout.as_secs()
            )
        })
}

/// Poll `cond` every [`POLL_INTERVAL`] until it is true or `timeout` elapses.
async fn wait_until(timeout: Duration, cond: impl Fn() -> bool) -> Result<(), ()> {
    let deadline = Instant::now() + timeout;
    loop {
        if cond() {
            return Ok(());
        }
        if Instant::now() >= deadline {
            return Err(());
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// Whether a device with `serial` is currently enumerated.
fn serial_present(serial: &str) -> bool {
    discover_devices()
        .map(|devs| devs.iter().any(|d| d.serial.as_deref() == Some(serial)))
        .unwrap_or(false)
}

/// Heuristic: adb-over-tcpip serials are `host:port` (contain a `:`), whereas
/// USB serials do not. Used to exclude tcpip devices from reboot-recovery.
fn is_tcpip_serial(serial: &str) -> bool {
    serial.contains(':')
}

/// Prompt the operator with a yes/no question on stdin (default: no).
fn prompt_yes_no(question: &str) -> bool {
    use std::io::Write as _;
    print!("{question} [y/N]: ");
    let _ = std::io::stdout().flush();
    let mut line = String::new();
    // Blocking stdin read is fine here: the interactive phase is explicitly
    // human-paced, so we are not starving any concurrent async work.
    if std::io::stdin().read_line(&mut line).is_err() {
        return false;
    }
    matches!(line.trim().to_ascii_lowercase().as_str(), "y" | "yes")
}

/// Record an interactive case result (start + finish lines).
fn record(reporter: &mut Reporter, suite: &str, name: &str, outcome: Outcome) {
    let full = format!("{suite}.{name}");
    Reporter::start_case(&full);
    reporter.finish_case(CaseResult {
        suite: suite.to_string(),
        name: name.to_string(),
        outcome,
        elapsed: Duration::from_millis(0),
    });
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn tcpip_serial_detection() {
        assert!(is_tcpip_serial("192.168.1.10:5555"), "host:port is tcpip");
        assert!(!is_tcpip_serial("ABCDEF0123"), "bare serial is USB");
    }

    #[tokio::test]
    async fn wait_until_times_out_when_condition_never_true() {
        // A 0-length timeout with an always-false condition must return Err fast.
        let r = wait_until(Duration::from_millis(0), || false).await;
        assert!(r.is_err(), "never-true condition must time out");
    }

    #[tokio::test]
    async fn wait_until_returns_ok_when_condition_true() {
        let r = wait_until(Duration::from_secs(5), || true).await;
        assert!(
            r.is_ok(),
            "already-true condition must return Ok immediately"
        );
    }
}
