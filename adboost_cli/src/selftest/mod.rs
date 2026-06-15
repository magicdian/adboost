//! Interactive, device-backed self-test harness for adboost.
//!
//! `adboost_cli selftest` runs against the **real** ADB devices connected to the
//! host. It first runs a fully automated suite (no user interaction) over two
//! channels — USB-direct and through adboost's own in-process server — then (in
//! the interactive phase) prompts for physical actions like USB re-plug.
//!
//! Results are reported in a gtest-style format (see [`report`]); the process
//! exit status reflects success so the harness is CI/script friendly.
//!
//! # Phasing
//!
//! 1. **Discover** — enumerate USB devices; abort with a clear message if none.
//! 2. **Automated** — for each addressable device, run the case suite on each
//!    channel. tcpip devices are reported as SKIPPED (pre-wired, see PRD).
//! 3. **Interactive** — (PR6) re-plug / reboot recovery, gated behind a prompt.

mod cases;
mod channels;
mod interactive;
mod parity;
mod report;
mod reverse_cases;

use std::time::{Duration, Instant};

use adb_client::ADBDeviceExt;
use adb_client::proxy::ADBProxyDevice;
use adb_client::usb::PersistentUsbConnection;

use crate::models::{ADBCliError, ADBCliResult, SelftestCommand};
use channels::{DiscoveredDevice, InProcessServer, discover_devices};
use report::{CaseResult, Outcome, Reporter};

/// Run the self-test harness end to end.
///
/// # Errors
///
/// Returns an error only for harness-level failures (e.g. no device connected).
/// Individual test-case failures are reported per-case and surfaced through a
/// non-success process exit, not through this `Err`.
pub async fn run(cmd: SelftestCommand) -> ADBCliResult<()> {
    let devices = discover_devices().map_err(ADBCliError::from)?;
    if devices.is_empty() {
        return Err(ADBCliError::from(
            "no ADB device detected — connect a device (and authorize it) before running selftest"
                .to_string(),
        ));
    }

    let addressable: Vec<&DiscoveredDevice> =
        devices.iter().filter(|d| d.serial.is_some()).collect();
    println!(
        "Detected {} ADB device(s); {} addressable by serial.",
        devices.len(),
        addressable.len()
    );
    for d in &devices {
        let serial = d.serial.as_deref().unwrap_or("<no-serial>");
        println!("  - {serial}  ({})", d.description);
    }
    if addressable.is_empty() {
        return Err(ADBCliError::from(
            "no device exposes a serial; cannot address any device for testing".to_string(),
        ));
    }

    let multi = addressable.len() > 1;
    println!(
        "Scenario: {} device.",
        if multi { "multi" } else { "single" }
    );

    let started = Instant::now();
    let mut reporter = Reporter::new();

    // The automated suite runs ~5 cases per channel × 2 channels per device.
    Reporter::start_run(addressable.len() * estimated_cases_per_device());

    // Phase ordering is deliberate around USB's single-exclusive-claim rule:
    // run EVERY usb_direct case for ALL devices first (each opens+drops a fresh
    // claim), and only THEN stand up the in-process server (whose backend caches
    // a persistent per-device claim). Interleaving the two contends for the same
    // device and yields CLSE/CNXN-instead-of-OKAY errors.
    let serials: Vec<String> = addressable
        .iter()
        .map(|d| d.serial.clone().expect("addressable ⇒ has serial"))
        .collect();

    for (idx, serial) in serials.iter().enumerate() {
        // Settle between devices: the prior device's connection released its USB
        // claim on close, and OS USB stacks (macOS IOKit especially) need a beat
        // to fully retire the endpoints before the next claim. Claiming too
        // eagerly back-to-back can abort the fresh claim mid-handshake
        // (kIOReturnAborted / 0xe00002ed), killing that connection's reader task.
        if idx > 0 {
            tokio::time::sleep(USB_DIRECT_INTER_DEVICE_SETTLE).await;
        }
        run_usb_direct_suite(&mut reporter, serial).await;
    }
    run_through_server_phase(&mut reporter, &serials, multi).await;

    // tcpip pre-wired placeholder (no tcpip detection wired yet — see PRD).
    report_tcpip_skipped(&mut reporter);

    if cmd.no_interactive {
        println!("Skipping interactive phase (--no-interactive).");
    } else {
        interactive::run_interactive_phase(&mut reporter, &addressable).await;
    }

    let ok = reporter.finish_run(started.elapsed());
    if ok {
        Ok(())
    } else {
        Err(ADBCliError::from(
            "selftest completed with failing cases".to_string(),
        ))
    }
}

/// Approximate automated cases per device, for the run banner count only:
/// 3 `usb_direct` + 5 `through_server` + forward + parity ≈ 10. The banner is an
/// estimate; the final summary reports exact counts.
fn estimated_cases_per_device() -> usize {
    10
}

/// Run the through-server phase for every device on ONE shared in-process
/// server. Standing the server up once (rather than per device) keeps a single,
/// stable set of cached device claims for the whole phase.
async fn run_through_server_phase(reporter: &mut Reporter, serials: &[String], multi: bool) {
    // Give the last usb_direct claim a beat to release before the server backend
    // claims the devices.
    tokio::time::sleep(Duration::from_millis(200)).await;

    let server = match InProcessServer::start().await {
        Ok(s) => s,
        Err(e) => {
            for serial in serials {
                let _ = serial;
                skip_suite(
                    reporter,
                    "through_server",
                    &format!("cannot start in-process server: {e}"),
                );
            }
            return;
        }
    };

    for serial in serials {
        // Select by serial when multiple devices are present (the `-s`
        // equivalent); otherwise autodetect the single device.
        let mut device = if multi {
            ADBProxyDevice::new(serial.clone(), Some(server.addr()))
        } else {
            ADBProxyDevice::autodetect(Some(server.addr()))
        };
        run_suite(reporter, "through_server", &mut device).await;
        // forward is a host-protocol feature (not an `ADBDeviceExt` method), so
        // it is exercised only on the through-server channel.
        let outcome = case_forward_control_plane(&mut device).await;
        run_one(reporter, "through_server", "forward_add_remove", outcome);

        // Reverse data-plane: echo always (when the device has nc), iperf3 when
        // present. A fixed device-listen port per device avoids collisions.
        run_reverse_cases(reporter, &mut device).await;

        // Optional parity: drive the SAME adboost server with the official `adb`
        // client (auto-detected; SKIPPED when adb is absent).
        run_parity_against_server(reporter, server.addr(), serial).await;
    }
    // Ambiguous-selection parity: only meaningful with >1 device, and it tests
    // ambiguity across the whole device set, so it runs ONCE (not per serial).
    if multi {
        run_ambiguous_parity_against_server(reporter, server.addr()).await;
    }
    // Gracefully shut the server down: flush a connection-level CLSE to every
    // cached device connection while their writer tasks are still alive, then
    // free the port. This prevents orphaned device streams that would otherwise
    // make a subsequent run's `usb_direct` CNXN hit a stale CLSE. (A bare drop
    // would only abort the accept loop, not flush the CLSEs.)
    server.shutdown().await;
}

/// Settle delay between two devices' `usb_direct` suites, giving the OS USB stack
/// time to retire the prior device's released claim before the next one is
/// claimed (avoids back-to-back claim aborts; see the loop in `run`).
const USB_DIRECT_INTER_DEVICE_SETTLE: Duration = Duration::from_millis(300);

/// Device-listen port used by the reverse data-plane cases (arbitrary high port
/// unlikely to be in use on the device).
const REVERSE_DEVICE_PORT: u16 = 47131;

/// Run the reverse data-plane cases against `device` (through-server channel):
/// `reverse_echo` (when the device has `nc`) then `reverse_iperf3` (when the
/// device has `iperf3`). Each is SKIPPED with a reason when its tool is absent.
async fn run_reverse_cases(reporter: &mut Reporter, device: &mut ADBProxyDevice) {
    if reverse_cases::device_has_nc(device).await {
        let outcome = reverse_cases::reverse_echo(device, REVERSE_DEVICE_PORT).await;
        run_one(reporter, "through_server", "reverse_echo", outcome);
    } else {
        run_one(
            reporter,
            "through_server",
            "reverse_echo",
            Outcome::Skipped("device has no `nc` for the reverse echo client".into()),
        );
    }

    if reverse_cases::device_has_iperf3(device).await {
        let outcome = reverse_cases::reverse_iperf3(device, REVERSE_DEVICE_PORT).await;
        run_one(reporter, "through_server", "reverse_iperf3", outcome);
    } else {
        run_one(
            reporter,
            "through_server",
            "reverse_iperf3",
            Outcome::Skipped("device has no `iperf3`".into()),
        );
    }
}

/// Run the official-adb parity case against the running adboost server, or
/// SKIP the group when no `adb` binary is available.
async fn run_parity_against_server(
    reporter: &mut Reporter,
    addr: std::net::SocketAddrV4,
    serial: &str,
) {
    if parity::adb_available().await {
        let outcome = parity::case_official_adb_shell(addr, serial).await;
        run_one(reporter, "parity", "official_adb_shell", outcome);
    } else {
        run_one(
            reporter,
            "parity",
            "official_adb_shell",
            Outcome::Skipped("official `adb` binary not found on PATH".into()),
        );
    }
}

/// Run the ambiguous-selection parity case (no `-s` against multiple devices)
/// against the running adboost server, or SKIP when no `adb` binary is
/// available. Runs once per multi-device run; never emitted in single-device
/// mode (where it is meaningless).
async fn run_ambiguous_parity_against_server(
    reporter: &mut Reporter,
    addr: std::net::SocketAddrV4,
) {
    if parity::adb_available().await {
        let outcome = parity::case_official_adb_ambiguous_shell(addr).await;
        run_one(reporter, "parity", "official_adb_ambiguous_shell", outcome);
    } else {
        run_one(
            reporter,
            "parity",
            "official_adb_ambiguous_shell",
            Outcome::Skipped("official `adb` binary not found on PATH".into()),
        );
    }
}

/// Forward control-plane case (through-server only): add an auto-assigned
/// forward rule via the proxy client, then remove it. This drives the server's
/// `host:forward` / `host:killforward` family end-to-end through a real client.
///
/// We validate the control plane (rule add/remove succeed) rather than the data
/// plane: a data-plane test needs a known device-side listener, which is not
/// guaranteed on an arbitrary device.
async fn case_forward_control_plane(device: &mut ADBProxyDevice) -> Outcome {
    // `forward(remote, local)` — local `tcp:0` lets the OS assign the host port.
    if let Err(e) = device
        .forward("tcp:5555".to_string(), "tcp:0".to_string())
        .await
    {
        return Outcome::Failed(format!("forward add failed: {e}"));
    }
    // Remove all rules (we don't know the assigned local port; remove-all is the
    // robust teardown and also exercises killforward-all).
    match device.forward_remove_all().await {
        Ok(()) => Outcome::Passed,
        Err(e) => Outcome::Failed(format!("forward remove-all failed: {e}")),
    }
}

/// Run the standard suite over the USB-direct channel.
///
/// The USB-direct channel uses a [`PersistentUsbConnection`] (NOT the
/// non-persistent `ADBUSBDevice`): it multiplexes every case over ONE
/// authenticated connection and sends a connection-level CLSE on drop, so
/// several sequential services run cleanly. (A reused `ADBUSBDevice`, or one
/// re-opened per case, races adbd's endpoint teardown and reads stale frames —
/// observed on real devices.) This also exercises the exact primitive the
/// server backend rides on. The connection is closed before returning so its
/// USB claim is released for the through-server phase.
async fn run_usb_direct_suite(reporter: &mut Reporter, serial: &str) {
    let conn = match PersistentUsbConnection::new_from_serial(serial, None).await {
        Ok(c) => c,
        Err(e) => {
            skip_suite(
                reporter,
                "usb_direct",
                &format!("cannot open USB device: {e}"),
            );
            return;
        }
    };

    run_one(
        reporter,
        "usb_direct",
        "shell_echo",
        guarded_persistent_case(&conn, cases::persistent_shell_echo(&conn)).await,
    );
    run_one(
        reporter,
        "usb_direct",
        "shell_v2",
        guarded_persistent_case(&conn, cases::persistent_shell_v2(&conn)).await,
    );
    run_one(
        reporter,
        "usb_direct",
        "push_pull_roundtrip",
        guarded_persistent_case(&conn, cases::persistent_push_pull(&conn)).await,
    );

    // Gracefully close (flush a connection-level CLSE) before the claim is
    // needed by the through-server phase.
    conn.close().await;
}

/// Connection-level failure reason for the `usb_direct` suite when the persistent
/// connection's reader task has exited (e.g. the OS aborted the USB device claim
/// — macOS `IOKit` `kIOReturnAborted` / `0xe00002ed` — under back-to-back
/// multi-device claims). Used both to short-circuit a case and to annotate a
/// case that failed *because* the reader died mid-run.
const READER_DEAD_REASON: &str =
    "persistent connection died: USB reader task exited (e.g. the OS aborted the \
device claim). The remaining usb_direct cases cannot run on this connection.";

/// Run one `usb_direct` case with connection-liveness guarding so a dead reader
/// task surfaces as ONE clear connection-level failure rather than N confusing
/// `error sending data to channel` per-case errors.
///
/// - If the reader is already dead, the case future is dropped un-awaited and the
///   outcome is the connection-level reason.
/// - Otherwise the case runs; if it then fails AND the reader has since died, the
///   failure is annotated with the connection-level root cause (the underlying
///   error is kept for diagnosis).
///
/// `case` is taken as an un-awaited future so the liveness check can short-circuit
/// it (async fns are lazy — nothing runs until awaited).
async fn guarded_persistent_case(
    conn: &PersistentUsbConnection,
    case: impl std::future::Future<Output = Outcome>,
) -> Outcome {
    // Pre-check: a dead reader means the case cannot run — short-circuit without
    // awaiting it (async fns are lazy, so the case body never executes).
    if let Some(reason) = reader_dead_short_circuit(conn.is_alive()) {
        return reason;
    }
    let outcome = case.await;
    // Post-check: annotate a failure that happened because the reader died mid-run.
    annotate_if_reader_died(conn.is_alive(), outcome)
}

/// Pure policy: if the connection's reader is not alive *before* a case, the case
/// cannot run — return the connection-level failure to use in its place.
/// `None` means "reader alive, run the case normally". Sans-io / unit-testable.
fn reader_dead_short_circuit(alive_before: bool) -> Option<Outcome> {
    (!alive_before).then(|| Outcome::Failed(READER_DEAD_REASON.to_string()))
}

/// Pure policy: given the case `outcome` and whether the reader is still alive
/// *after* it ran, annotate a failure that was actually caused by the reader
/// dying mid-case (its per-case error is just a symptom). A passing/skipped
/// outcome, or a genuine failure with the reader still alive, passes through
/// unchanged. Sans-io / unit-testable.
fn annotate_if_reader_died(alive_after: bool, outcome: Outcome) -> Outcome {
    match outcome {
        Outcome::Failed(reason) if !alive_after => {
            Outcome::Failed(format!("{READER_DEAD_REASON} (underlying: {reason})"))
        }
        other => other,
    }
}

/// Run the standard automated case suite against `device`, recording each
/// result under `suite`.
async fn run_suite<D: ADBDeviceExt>(reporter: &mut Reporter, suite: &str, device: &mut D) {
    run_one(
        reporter,
        suite,
        "shell_echo",
        cases::case_shell_echo(device).await,
    );
    run_one(
        reporter,
        suite,
        "shell_exit_code",
        cases::case_shell_exit_code(device).await,
    );
    run_one(
        reporter,
        suite,
        "push_pull_roundtrip",
        cases::case_push_pull_roundtrip(device).await,
    );
    run_one(
        reporter,
        suite,
        "list_scratch_dir",
        cases::case_list_scratch_dir(device).await,
    );
    run_one(
        reporter,
        suite,
        "stat_root",
        cases::case_stat_root(device).await,
    );
}

/// Record a single completed case (start/finish lines + result). Timing wraps
/// the whole start→finish window; per-case fine timing is not load-bearing.
fn run_one(reporter: &mut Reporter, suite: &str, name: &str, outcome: Outcome) {
    let full = format!("{suite}.{name}");
    Reporter::start_case(&full);
    reporter.finish_case(CaseResult {
        suite: suite.to_string(),
        name: name.to_string(),
        outcome,
        elapsed: Duration::from_millis(0),
    });
}

/// Emit SKIPPED for the whole standard suite when a channel can't be opened.
fn skip_suite(reporter: &mut Reporter, suite: &str, reason: &str) {
    for name in [
        "shell_echo",
        "shell_exit_code",
        "push_pull_roundtrip",
        "list_scratch_dir",
        "stat_root",
    ] {
        run_one(reporter, suite, name, Outcome::Skipped(reason.to_string()));
    }
}

/// tcpip support is pre-wired but not implemented (no debug environment yet).
/// Report a single SKIPPED placeholder so the intent is visible in output.
fn report_tcpip_skipped(reporter: &mut Reporter) {
    run_one(
        reporter,
        "tcpip",
        "shell_echo",
        Outcome::Skipped(
            "tcpip channel not implemented yet (pre-wired; pending emulator debug env)".into(),
        ),
    );
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn dead_reader_before_case_short_circuits_to_connection_failure() {
        // Reader already dead: the case must be replaced by the connection-level
        // reason (and the caller drops the case future un-awaited).
        let out = reader_dead_short_circuit(false).expect("dead reader short-circuits");
        match out {
            Outcome::Failed(reason) => assert!(
                reason.contains("USB reader task exited"),
                "reason names the reader death: {reason}"
            ),
            _ => panic!("dead reader must short-circuit to Failed"),
        }
    }

    #[test]
    fn live_reader_before_case_runs_normally() {
        // Reader alive: no short-circuit, the case is run as usual.
        assert!(
            reader_dead_short_circuit(true).is_none(),
            "a live reader must not short-circuit the case"
        );
    }

    #[test]
    fn failure_after_reader_death_is_annotated_with_root_cause() {
        let symptom = Outcome::Failed("error sending data to channel".to_string());
        match annotate_if_reader_died(false, symptom) {
            Outcome::Failed(reason) => {
                assert!(
                    reason.contains("USB reader task exited"),
                    "names the real cause: {reason}"
                );
                assert!(
                    reason.contains("error sending data to channel"),
                    "keeps the underlying symptom: {reason}"
                );
            }
            _ => panic!("a failure with a dead reader must stay Failed (annotated)"),
        }
    }

    #[test]
    fn genuine_failure_with_live_reader_passes_through_unchanged() {
        let genuine = Outcome::Failed("echo returned \"nope\"".to_string());
        match annotate_if_reader_died(true, genuine) {
            Outcome::Failed(reason) => {
                assert_eq!(
                    reason, "echo returned \"nope\"",
                    "a real failure with a live reader must NOT be annotated"
                );
            }
            _ => panic!("must remain the original Failed"),
        }
    }

    #[test]
    fn passing_outcome_is_never_annotated() {
        // Even if the reader is reported not-alive after a pass (e.g. a benign
        // race at teardown), a Passed/Skipped outcome passes through untouched.
        assert_eq!(annotate_if_reader_died(false, Outcome::Passed), Outcome::Passed);
        let skip = Outcome::Skipped("n/a".to_string());
        assert_eq!(annotate_if_reader_died(false, skip.clone()), skip);
    }
}
