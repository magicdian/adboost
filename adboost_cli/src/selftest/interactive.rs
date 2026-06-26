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
use adboost::RebootType;
use adboost::usb::PersistentUsbConnection;

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

    // Ordering invariant: `reboot_recovery` MUST run last. It reboots the device,
    // which is slow and leaves the device not-yet-ready for a while afterwards, so
    // any case running after it would race device recovery and flake. Add new
    // interactive/tcpip cases ABOVE the reboot block, never below it.
    let replug = case_usb_replug(&subject).await;
    record(reporter, "interactive", "usb_replug", replug);

    let forward_release = case_usb_forward_release_on_unplug(&subject).await;
    record(
        reporter,
        "interactive",
        "forward_release_on_unplug",
        forward_release,
    );

    let pty_hup = case_pty_hup_kills_process_group(&subject).await;
    record(reporter, "interactive", "pty_hup_process_group", pty_hup);

    let tcpip = case_tcpip_through_server(&subject).await;
    record(reporter, "tcpip", "shell_through_tcp_device", tcpip);

    // NOTE: the root → unroot cycle is NOT part of the interactive phase — it is an
    // automated case run THROUGH the in-process server (see
    // `cases::case_root_unroot_cycle`, wired by `run_through_server_phase`). Do not
    // re-add it here.

    // ALWAYS LAST — see the ordering invariant above.
    let reboot = case_reboot_recovery(&subject).await;
    record(reporter, "interactive", "reboot_recovery", reboot);
}

/// PTY-HUP process-group kill: open a PTY-allocated `shell,v2` session running a
/// long-lived child (`sleep`), record the child PID, then DROP the session. The
/// host close tears the ADB stream down; on the device the PTY master closes and
/// the kernel delivers `SIGHUP` to the **entire foreground process group**, so
/// the `sleep` child must be gone when we check from a fresh shell.
///
/// This is the load-bearing mechanism behind "local disconnect/cancel → device
/// process gets a signal and exits" (the v1 path could only CLSE the ADB stream,
/// which does NOT signal the remote process group). It is hardware/kernel
/// behavior no unit test can assert — hence operator-gated and live-device only.
/// On MTK 8676 / Android 16 the PTY-HUP-reaches-the-group property is the one
/// this case proves.
async fn case_pty_hup_kills_process_group(serial: &str) -> Outcome {
    use adboost::ShellV2Service;

    if !prompt_yes_no(&format!(
        "[pty_hup] Open a PTY shell on {serial}, start a `sleep`, then drop the session and \
         verify the kernel SIGHUPs the process group? (non-destructive)"
    )) {
        return Outcome::Skipped("operator declined PTY-HUP case".into());
    }

    let conn = match PersistentUsbConnection::new_from_serial(serial, None).await {
        Ok(c) => c,
        Err(e) => return Outcome::Failed(format!("cannot open device: {e}")),
    };

    // A unique marker so we can find (and clean up) exactly our child.
    let marker = "adboost_pty_hup_probe_3c7e";
    // Spawn a long sleep tagged with the marker as an argument, print a ready
    // line, then block so the shell stays the PTY foreground process group.
    // `exec` so the sleep IS the foreground process (the PTY's controlling group).
    let pty_cmd = format!("echo READY; exec sleep 3600 {marker}");

    let mut session = match conn
        .open_shell_v2_service(ShellV2Service::new(&pty_cmd).with_pty())
        .await
    {
        Ok(s) => s,
        Err(e) => {
            conn.close().await;
            return Outcome::Failed(format!("open PTY shell,v2 failed: {e}"));
        }
    };

    // Wait until the shell signals READY (so the sleep is actually running)
    // before we cancel — otherwise we might drop before the child exists.
    if let Err(e) = wait_for_ready_frame(&mut session).await {
        conn.close().await;
        return Outcome::Failed(format!("PTY shell never became READY: {e}"));
    }

    // Confirm the child IS running right now (sanity: the probe is valid).
    let running_before = pgrep_marker(&conn, marker).await;
    if !running_before {
        drop(session);
        conn.close().await;
        return Outcome::Skipped(
            "sleep child not observed before cancel; cannot validate PTY-HUP".into(),
        );
    }

    // THE CANCEL: drop the session. Host-side close → PTY master closes on the
    // device → kernel SIGHUPs the foreground process group (the sleep).
    drop(session);

    // Poll for the child to disappear. If PTY-HUP reaches the group, the sleep
    // dies promptly; allow a short window for adbd + kernel to deliver it.
    let deadline = Instant::now() + Duration::from_secs(10);
    let mut still_running = true;
    while Instant::now() < deadline {
        if !pgrep_marker(&conn, marker).await {
            still_running = false;
            break;
        }
        sleep(POLL_INTERVAL).await;
    }

    if still_running {
        // Clean up the leaked child so the rig is left tidy, then fail.
        let _ = conn.shell_exec(&format!("pkill -f {marker}")).await;
        conn.close().await;
        return Outcome::Failed(format!(
            "PTY-HUP did NOT reach the process group: `sleep {marker}` still running 10s after \
             session drop (expected SIGHUP on PTY master close)"
        ));
    }

    conn.close().await;
    Outcome::Passed
}

/// Read shell-v2 frames until a stdout frame contains `READY`, or the stream
/// ends / a bounded number of frames pass without it.
async fn wait_for_ready_frame(session: &mut adboost::usb::ShellV2Session) -> Result<(), String> {
    use adboost::usb::ShellChannel;

    for _ in 0..64 {
        match session.read_frame().await {
            Ok(Some(frame)) => {
                if frame.channel == ShellChannel::Stdout
                    && String::from_utf8_lossy(&frame.payload).contains("READY")
                {
                    return Ok(());
                }
            }
            Ok(None) => return Err("stream closed before READY".into()),
            Err(e) => return Err(format!("read_frame error: {e}")),
        }
    }
    Err("READY not seen within 64 frames".into())
}

/// Whether a process whose command line contains `marker` is currently running,
/// via a fresh `pgrep -f` shell on the connection.
async fn pgrep_marker(conn: &PersistentUsbConnection, marker: &str) -> bool {
    // `pgrep -f` matches against the full command line (so it finds `sleep 3600
    // <marker>`). Exit code is non-zero when nothing matches; we key on stdout.
    match conn.shell_exec(&format!("pgrep -f {marker}")).await {
        Ok((out, _)) => out.split_whitespace().any(|t| t.parse::<u32>().is_ok()),
        Err(_) => false,
    }
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
        .open_session(&adboost::ADBLocalCommand::TcpIp(TCPIP_PORT))
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

    // Best-effort readiness gate: `restore_usb_mode` issues `usb:` fire-and-forget,
    // which restarts adbd and triggers a USB re-enumeration. Without waiting, the
    // next case (`case_reboot_recovery`) opens the device within ~2s while adbd is
    // still coming back, and the CNXN handshake fails. After switching back to USB
    // the device re-enumerates under its ORIGINAL USB serial (not `tcp_serial`), so
    // wait on `serial`. This MUST NOT change `outcome` — the tcpip conclusion was
    // already computed by `run_tcpip_through_server`; failures here only warn.
    if let Err(e) = wait_for_presence(serial, RECONNECT_TIMEOUT).await {
        tracing::warn!("[tcpip] device {serial} did not re-enumerate over USB after restore: {e}");
    } else if let Err(e) = open_device_with_retry(serial, Duration::from_secs(20)).await {
        tracing::warn!("[tcpip] device {serial} returned but is not yet ready after restore: {e}");
    }
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
    match adboost::tcp::ADBTcpDevice::new(addr).await {
        Ok(mut dev) => {
            use adboost::ADBDeviceExt as _;
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

/// USB-unplug forward release: with adboost serving as an ADB server (default
/// `OnDisconnect::ReleaseAll`), register a `forward` for a USB device through the
/// **official** `adb -P` client, then physically unplug the device and assert the
/// rule disappears from `forward --list` automatically — exactly what standard
/// `adb` does and what the caller reported as missing.
///
/// This is the one end-to-end path the contract (mock-event) tests cannot reach:
/// the real `nusb` hotplug → diff → `LifecycleEvent::Disconnected` → `handle_disconnects`
/// → `ForwardHandle::release` chain. Hardware-only + operator-gated.
async fn case_usb_forward_release_on_unplug(serial: &str) -> Outcome {
    /// Remote port the forward targets on the device (arbitrary; never dialed).
    const REMOTE_PORT: u16 = 5555;
    /// How long to wait, after unplug, for the rule to vanish from `forward --list`.
    const RELEASE_TIMEOUT: Duration = Duration::from_secs(30);

    if is_tcpip_serial(serial) {
        return Outcome::Skipped("subject is a tcpip device, not a USB-unplug subject".into());
    }
    if !parity::adb_available().await {
        return Outcome::Skipped("adb not on PATH".into());
    }

    let server = match InProcessServer::start().await {
        Ok(s) => s,
        Err(e) => return Outcome::Skipped(format!("cannot start in-process server: {e}")),
    };
    let port = server.addr().port().to_string();
    let remote = format!("tcp:{REMOTE_PORT}");

    // Register a forward (tcp:0 ⇒ OS picks the local port) for this device.
    let register = adb_forward(&port, serial, "tcp:0", &remote).await;
    if let Err(e) = register {
        server.shutdown().await;
        return Outcome::Failed(format!("could not register forward: {e}"));
    }

    // Precondition: the rule must be present in `forward --list` before unplug.
    match adb_forward_list(&port).await {
        Ok(list) if list.lines().any(|l| l.contains(serial)) => {}
        Ok(list) => {
            server.shutdown().await;
            return Outcome::Failed(format!(
                "forward registered but {serial} absent from `forward --list`: {}",
                list.trim()
            ));
        }
        Err(e) => {
            server.shutdown().await;
            return Outcome::Failed(format!("could not read `forward --list`: {e}"));
        }
    }

    println!();
    println!("[forward_release] Please UNPLUG the device {serial} now.");
    if let Err(e) = wait_for_absence(serial, RECONNECT_TIMEOUT).await {
        server.shutdown().await;
        return Outcome::Failed(e);
    }

    // Core assertion: after unplug, the rule must auto-release within the timeout.
    let outcome = match wait_for_forward_release(&port, serial, RELEASE_TIMEOUT).await {
        Ok(()) => Outcome::Passed,
        Err(last) => Outcome::Failed(format!(
            "BUG REPRODUCED: forward rule for {serial} still present {}s after unplug \
             (expected auto-release); last `forward --list`: {}",
            RELEASE_TIMEOUT.as_secs(),
            last.trim()
        )),
    };

    // Best-effort: ask the operator to re-plug so the rig returns to its prior
    // state. A failure here does not change the conclusion above.
    println!("[forward_release] Core check done. Please RE-PLUG the device {serial} to restore.");
    if let Err(e) = wait_for_presence(serial, RECONNECT_TIMEOUT).await {
        tracing::warn!("[forward_release] device {serial} did not return after replug: {e}");
    } else if let Outcome::Failed(reason) = verify_shell_after_recovery(serial).await {
        // Best-effort readiness gate: the device is enumerated but adbd may not
        // yet accept a CNXN handshake right after re-enumeration. Drive it to a
        // stable state (open connection + shell echo) so the next case
        // (`case_reboot_recovery`) does not open a not-yet-ready device. A
        // failure here MUST NOT change `outcome` (the core conclusion was
        // already computed at unplug time) — only warn.
        tracing::warn!(
            "[forward_release] device {serial} returned but did not stabilize after replug: {reason}"
        );
    }

    server.shutdown().await;
    outcome
}

/// Poll `adb -P <port> forward --list` until no line mentions `serial`, or time
/// out. On timeout returns the last observed listing (for the failure message).
async fn wait_for_forward_release(
    port: &str,
    serial: &str,
    timeout: Duration,
) -> Result<(), String> {
    let deadline = Instant::now() + timeout;
    loop {
        let last = match adb_forward_list(port).await {
            Ok(list) => {
                if !list.lines().any(|l| l.contains(serial)) {
                    return Ok(());
                }
                list
            }
            Err(e) => format!("(forward --list error: {e})"),
        };
        if Instant::now() >= deadline {
            return Err(last);
        }
        sleep(POLL_INTERVAL).await;
    }
}

/// `adb -P <port> -s <serial> forward <local> <remote>`; Err on a non-zero exit.
async fn adb_forward(port: &str, serial: &str, local: &str, remote: &str) -> Result<(), String> {
    let output = tokio::process::Command::new("adb")
        .args(["-P", port, "-s", serial, "forward", local, remote])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("could not invoke adb forward: {e}"))?;
    if output.status.success() {
        Ok(())
    } else {
        Err(format!(
            "adb forward exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ))
    }
}

/// `adb -P <port> forward --list`; returns stdout (the rule listing).
async fn adb_forward_list(port: &str) -> Result<String, String> {
    let output = tokio::process::Command::new("adb")
        .args(["-P", port, "forward", "--list"])
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::piped())
        .output()
        .await
        .map_err(|e| format!("could not invoke adb forward --list: {e}"))?;
    if output.status.success() {
        Ok(String::from_utf8_lossy(&output.stdout).into_owned())
    } else {
        Err(format!(
            "adb forward --list exited {}: {}",
            output.status,
            String::from_utf8_lossy(&output.stderr).trim_end()
        ))
    }
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
    //
    // Open the connection with a retry budget: a prior case (`case_tcpip_through_server`)
    // may have just switched the device back to USB, so adbd can still be mid-restart
    // when we get here. Only the OPEN is retried — once open, a `reboot()` failure is
    // a genuine failure and is surfaced directly (no reboot-itself retry).
    match open_device_with_retry(serial, Duration::from_secs(20)).await {
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

/// Open a persistent USB connection to `serial`, retrying within `budget` while
/// the device is still mid-(re)enumeration. Returns the open connection or the
/// last open error after the budget elapses.
///
/// Right after a USB re-enumeration (e.g. following `tcpip:`/`usb:` mode switches
/// or a reboot) adbd may not yet accept a CNXN handshake, so the bare
/// `new_from_serial` call fails transiently. Polling on [`POLL_INTERVAL`] within a
/// short budget converges past that window.
async fn open_device_with_retry(
    serial: &str,
    budget: Duration,
) -> Result<PersistentUsbConnection, String> {
    let deadline = Instant::now() + budget;
    loop {
        match PersistentUsbConnection::new_from_serial(serial, None).await {
            Ok(conn) => return Ok(conn),
            Err(e) if Instant::now() >= deadline => return Err(e.to_string()),
            Err(_) => sleep(POLL_INTERVAL).await,
        }
    }
}

/// After a device returns, open a persistent connection and run the shell-echo
/// case to prove the connection is usable again.
async fn verify_shell_after_recovery(serial: &str) -> Outcome {
    // The device may need a moment after re-enumeration before adbd is ready;
    // retry opening within a short budget.
    match open_device_with_retry(serial, Duration::from_secs(20)).await {
        Ok(conn) => {
            let outcome = cases::persistent_shell_echo(&conn).await;
            conn.close().await;
            outcome
        }
        Err(e) => Outcome::Failed(format!("device returned but shell not ready: {e}")),
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
