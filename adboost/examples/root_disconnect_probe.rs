//! Pressure-test / diagnostic harness for the `wait-for-disconnect` redesign
//! (task `06-23-wait-for-bug-okay-disconnect-presence-60s`, decision D2).
//!
//! It reproduces the **server scenario** that the real bug lives in: a single
//! long-lived `PersistentUsbConnection` (the analogue of the backend's cached
//! per-serial connection) over which we issue `root:` / `unroot:`, then observe
//! whether and how fast THAT connection's reader task dies when adbd restarts —
//! versus silently stalling. The whole Bug 2 event-driven design rests on the
//! premise "adbd close => reader dies fatally, promptly"; this measures it
//! instead of assuming it.
//!
//! Per cycle it captures the five unknowns that drive the redesign:
//!   1. Does `is_alive()` flip to false after the control service? (event premise)
//!   2. Latency: control-reply -> `is_alive()==false`. (sets the fallback bound)
//!   3. Did the death look fatal (alive=false fast) or a stall (stays alive)?
//!   4. Did the serial leave USB enumeration, and for how long? (presence-poll
//!      reliability — the thing the current code wrongly depends on)
//!   5. Reachability after: can a fresh connection shell again, and how soon?
//!
//! Run with logging so the reader-death WARN is visible and timestamped:
//!
//! ```text
//! RUST_LOG=adboost=debug cargo run -p adboost --features usb,tracing-init \
//!     --example root_disconnect_probe -- <serial> <cycles>
//! ```
//!
//! NOTE: read-only w.r.t. the codebase — pure public-API harness. It restarts
//! adbd on the device many times (root<->unroot); that is the point. Leaves the
//! device unrooted (an even number of toggles) on clean exit.

use std::time::{Duration, Instant};

use adboost::ADBLocalCommand;
use adboost::usb::{PersistentUsbConnection, find_all_connected_adb_devices};
use tokio::io::AsyncReadExt;

/// How long to keep probing `is_alive()` + enumeration after a control service
/// before declaring "no death observed" (a stall). Generous so we capture the
/// true death latency even if it is multiple seconds.
const OBSERVE_BUDGET: Duration = Duration::from_secs(15);
/// Poll cadence for `is_alive()` / enumeration during observation. Fine-grained
/// so the measured death latency is accurate to ~20ms.
const OBSERVE_POLL: Duration = Duration::from_millis(20);

#[derive(Debug, Default)]
struct CycleStat {
    service: &'static str,
    reply: String,
    /// ms from "control reply read" to `is_alive()==false`; None = never died.
    death_latency_ms: Option<u128>,
    /// ms the serial was ABSENT from USB enumeration during the window; 0 = never left.
    absent_ms: u128,
    /// did a fresh connection shell-echo succeed afterwards, and how long it took (ms)
    reopen_ms: Option<u128>,
    note: String,
}

#[tokio::main]
async fn main() {
    tracing_subscriber::fmt()
        .with_env_filter(
            tracing_subscriber::EnvFilter::try_from_default_env()
                .unwrap_or_else(|_| tracing_subscriber::EnvFilter::new("adboost=debug")),
        )
        .with_writer(std::io::stderr)
        .init();

    let args: Vec<String> = std::env::args().collect();
    let serial = args.get(1).cloned().unwrap_or_else(|| {
        eprintln!("usage: root_disconnect_probe <serial> [cycles=10]");
        std::process::exit(2);
    });
    let cycles: usize = args.get(2).and_then(|s| s.parse().ok()).unwrap_or(10);

    println!("# root/unroot disconnect probe — serial={serial} cycles={cycles}");
    println!("# columns: cycle service reply_first_line death_latency_ms absent_ms reopen_ms note");

    let mut stats = Vec::new();
    for i in 0..cycles {
        // Alternate root <-> unroot so we always toggle adbd identity.
        let (svc, cmd) = if i % 2 == 0 {
            ("root:", ADBLocalCommand::Root)
        } else {
            ("unroot:", ADBLocalCommand::Unroot)
        };
        match run_cycle(&serial, svc, &cmd).await {
            Ok(stat) => {
                println!(
                    "{i}\t{}\t{:?}\t{}\t{}\t{}\t{}",
                    stat.service,
                    stat.reply.lines().next().unwrap_or("").trim(),
                    stat.death_latency_ms
                        .map_or_else(|| "NONE".to_string(), |v| v.to_string()),
                    stat.absent_ms,
                    stat.reopen_ms
                        .map_or_else(|| "FAIL".to_string(), |v| v.to_string()),
                    stat.note,
                );
                stats.push(stat);
            }
            Err(e) => {
                println!("{i}\t{svc}\tERROR\t-\t-\t-\t{e}");
            }
        }
        // Let adbd settle fully before the next cycle so cycles are independent.
        tokio::time::sleep(Duration::from_secs(2)).await;
    }

    summarize(&stats);
}

/// One control-service cycle on a freshly opened, then KEPT-ALIVE connection
/// (mirrors the backend's cached connection). Returns the captured stat.
async fn run_cycle(
    serial: &str,
    svc: &'static str,
    cmd: &ADBLocalCommand,
) -> Result<CycleStat, String> {
    // Open the connection we will keep alive across the restart (the backend's
    // cached-connection analogue).
    let conn = PersistentUsbConnection::new_from_serial(serial, None)
        .await
        .map_err(|e| format!("open: {e}"))?;

    // Issue the control service and read its short textual reply to EOF (like a
    // shell read). open_session succeeding = adbd accepted the service.
    let mut session = conn
        .open_session(cmd)
        .await
        .map_err(|e| format!("open_session({svc}): {e}"))?;
    let mut reply = Vec::new();
    // Best-effort read: the restart may tear the stream down mid-reply.
    let _ = tokio::time::timeout(Duration::from_secs(3), session.read_to_end(&mut reply)).await;
    let reply = String::from_utf8_lossy(&reply).into_owned();
    drop(session);
    let t_reply = Instant::now();

    // Observe: poll is_alive() + USB enumeration until the reader dies or budget.
    let mut stat = CycleStat {
        service: svc,
        reply,
        ..Default::default()
    };
    let mut absent_since: Option<Instant> = None;
    let deadline = t_reply + OBSERVE_BUDGET;
    loop {
        if stat.death_latency_ms.is_none() && !conn.is_alive() {
            stat.death_latency_ms = Some(t_reply.elapsed().as_millis());
        }
        // Enumeration presence (what the buggy presence-poll watches).
        let present = find_all_connected_adb_devices()
            .map(|v| v.iter().any(|d| d.serial.as_deref() == Some(serial)))
            .unwrap_or(false);
        match (present, absent_since) {
            (false, None) => absent_since = Some(Instant::now()),
            (true, Some(since)) => {
                stat.absent_ms += since.elapsed().as_millis();
                absent_since = None;
            }
            _ => {}
        }
        // Stop once the reader has died AND enumeration is stable again, or budget.
        if (stat.death_latency_ms.is_some() && present && absent_since.is_none())
            || Instant::now() >= deadline
        {
            break;
        }
        tokio::time::sleep(OBSERVE_POLL).await;
    }
    if let Some(since) = absent_since {
        stat.absent_ms += since.elapsed().as_millis();
    }
    if stat.death_latency_ms.is_none() {
        stat.note = "NO_DEATH(stall?) — reader stayed alive within budget".to_string();
    } else if stat.absent_ms == 0 {
        stat.note = "died, serial never left enumeration (presence-poll would hang)".to_string();
    }
    conn.close().await;

    // Reachability: how soon can a fresh connection shell again?
    let t_reopen = Instant::now();
    match reopen_and_shell(serial).await {
        Ok(()) => stat.reopen_ms = Some(t_reopen.elapsed().as_millis()),
        Err(e) => stat.note = format!("{}; reopen failed: {e}", stat.note),
    }

    Ok(stat)
}

/// Open a fresh connection and confirm shell works (device fully back).
async fn reopen_and_shell(serial: &str) -> Result<(), String> {
    let deadline = Instant::now() + Duration::from_secs(20);
    loop {
        match PersistentUsbConnection::new_from_serial(serial, None).await {
            Ok(conn) => {
                let r = conn.shell_exec("echo probe_ok").await;
                conn.close().await;
                return match r {
                    Ok((out, _)) if out.contains("probe_ok") => Ok(()),
                    Ok((out, _)) => Err(format!("unexpected shell out: {out:?}")),
                    Err(e) => Err(format!("shell: {e}")),
                };
            }
            Err(_) if Instant::now() < deadline => {
                tokio::time::sleep(Duration::from_millis(200)).await;
            }
            Err(e) => return Err(format!("reopen: {e}")),
        }
    }
}

fn summarize(stats: &[CycleStat]) {
    let n = stats.len();
    if n == 0 {
        println!("\n# no successful cycles");
        return;
    }
    let died: Vec<u128> = stats.iter().filter_map(|s| s.death_latency_ms).collect();
    let stalls = n - died.len();
    let never_left = stats
        .iter()
        .filter(|s| s.death_latency_ms.is_some() && s.absent_ms == 0)
        .count();
    let reopen_fail = stats.iter().filter(|s| s.reopen_ms.is_none()).count();

    println!("\n# ===== SUMMARY ({n} cycles) =====");
    println!("# reader died (event premise holds): {}/{n}", died.len());
    println!("# reader STALLED (no death — would need liveness probe): {stalls}/{n}");
    if !died.is_empty() {
        let min = died.iter().min().unwrap();
        let max = died.iter().max().unwrap();
        let avg = died.iter().sum::<u128>() / died.len() as u128;
        println!("# death latency ms: min={min} avg={avg} max={max}  (sets fallback bound)");
    }
    println!(
        "# died but serial NEVER left enumeration: {never_left}/{n}  (proves presence-poll unreliable)"
    );
    println!("# reopen+shell failures: {reopen_fail}/{n}");
    println!("#");
    println!("# DECISION HINTS:");
    println!("#  - stalls==0  => event-on-reader-death is sufficient; no liveness probe needed.");
    println!("#  - stalls>0   => adbd close can silently stall; add a liveness probe.");
    println!(
        "#  - max death latency => pick fallback timeout comfortably above it (e.g. 2x, capped ~10s)."
    );
    println!(
        "#  - never_left>0 => confirms the current presence-poll is fundamentally broken for adb root."
    );
}
