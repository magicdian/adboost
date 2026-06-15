//! `server start` / `server kill` daemon control for the adboost ADB server.
//!
//! adboost's own ADB server (USB-backed, [`adboost::server`]) is long-lived,
//! unlike every other one-shot CLI command. `start` launches it as a detached
//! background process and records its PID; `kill` stops that process.
//!
//! # Why re-exec instead of fork
//!
//! `fork()` after the tokio runtime has spawned its worker threads is unsound
//! (only the calling thread survives in the child, leaving the runtime
//! deadlocked). So we never fork: `start` **re-execs a fresh `adboost` process**
//! with `Command::spawn`, which starts a brand-new process image (and its own
//! runtime) — safe. On Unix the child is detached into its own session via
//! `setsid` in a `pre_exec` hook and its stdio is redirected to a log file.

use std::net::SocketAddr;
use std::path::PathBuf;
use std::process::Stdio;
use std::sync::Arc;

use adboost::server::{AdbServerFrontend, DefaultDeviceBackend};
use tokio::process::Command;

use crate::models::{ADBCliError, ADBCliResult};

/// Marker env var set on the detached child so it knows it is the daemon body
/// (already detached; should write its PID and install the signal handler).
const DAEMON_ENV: &str = "ADBOOST_SERVER_DAEMON";

/// Resolve the PID-file path: explicit `--pid-file`, else a per-user default.
///
/// Default search: `$XDG_RUNTIME_DIR/adboost/server.pid` → `~/.android/adboost-server.pid`
/// → `<tmp>/adboost-server.pid`.
#[must_use]
pub fn resolve_pid_file(explicit: Option<PathBuf>) -> PathBuf {
    if let Some(p) = explicit {
        return p;
    }
    if let Ok(runtime_dir) = std::env::var("XDG_RUNTIME_DIR")
        && !runtime_dir.is_empty()
    {
        return PathBuf::from(runtime_dir)
            .join("adboost")
            .join("server.pid");
    }
    if let Some(home) = home_dir() {
        return home.join(".android").join("adboost-server.pid");
    }
    std::env::temp_dir().join("adboost-server.pid")
}

/// The companion log-file path next to the PID file (`server.log`).
#[must_use]
pub fn resolve_log_file(explicit: Option<PathBuf>, pid_file: &std::path::Path) -> PathBuf {
    explicit.unwrap_or_else(|| pid_file.with_file_name("adboost-server.log"))
}

fn home_dir() -> Option<PathBuf> {
    std::env::var_os("HOME").map(PathBuf::from)
}

/// Read the PID file, returning the PID only if a live process owns it. A stale
/// file (process gone) yields `None` so callers treat the server as not running.
#[must_use]
pub fn running_pid(pid_file: &std::path::Path) -> Option<u32> {
    let pid: u32 = std::fs::read_to_string(pid_file)
        .ok()?
        .trim()
        .parse()
        .ok()?;
    if is_alive(pid) { Some(pid) } else { None }
}

/// Whether `pid` is a live process (`kill(pid, 0)` on Unix).
#[cfg(unix)]
fn is_alive(pid: u32) -> bool {
    let Ok(pid) = libc::pid_t::try_from(pid) else {
        return false;
    };
    // SAFETY: `kill` with signal 0 performs only the existence/permission check
    // and delivers no signal; it has no memory-safety implications.
    unsafe { libc::kill(pid, 0) == 0 }
}

#[cfg(not(unix))]
fn is_alive(_pid: u32) -> bool {
    // Best-effort on non-Unix: assume a present PID file means running. The
    // primary supported platforms are macOS/Linux.
    true
}

/// Write `pid` to `pid_file`, creating parent directories as needed.
fn write_pid_file(pid_file: &std::path::Path, pid: u32) -> std::io::Result<()> {
    if let Some(parent) = pid_file.parent() {
        std::fs::create_dir_all(parent)?;
    }
    std::fs::write(pid_file, pid.to_string())
}

/// `server start`: launch (or report) the background ADB server.
///
/// With `foreground`, runs the server in this process until SIGTERM. Otherwise
/// spawns a detached child running the same binary in daemon mode, records its
/// PID, and returns immediately.
pub async fn start(
    address: SocketAddr,
    foreground: bool,
    pid_file: Option<PathBuf>,
    log_file: Option<PathBuf>,
) -> ADBCliResult<()> {
    let pid_file = resolve_pid_file(pid_file);

    // The detached child re-enters here with DAEMON_ENV set. It must NOT run the
    // already-running check: the parent has just written *its* pid into the file,
    // so the child would see a live pid (itself) and exit. The child goes
    // straight to serving (claiming the file with its own pid first).
    let is_daemon_child = std::env::var_os(DAEMON_ENV).is_some();

    if foreground || is_daemon_child {
        if is_daemon_child {
            // We are the detached child: claim the PID file with our own PID.
            write_pid_file(&pid_file, std::process::id())
                .map_err(|e| ADBCliError::from(format!("cannot write pid file: {e}")))?;
        }
        let result = run_server(address, &pid_file, foreground).await;
        // Best-effort cleanup so a clean exit does not leave a stale PID file.
        let _ = std::fs::remove_file(&pid_file);
        return result;
    }

    // Parent (interactive) path: refuse to start a second server.
    if let Some(pid) = running_pid(&pid_file) {
        tracing::info!("adboost server already running (pid {pid})");
        return Ok(());
    }

    // Parent: spawn a detached child in daemon mode, then record its PID.
    let log_path = resolve_log_file(log_file, &pid_file);
    if let Some(parent) = log_path.parent() {
        let _ = std::fs::create_dir_all(parent);
    }
    let log = std::fs::OpenOptions::new()
        .create(true)
        .append(true)
        .open(&log_path)
        .map_err(|e| {
            ADBCliError::from(format!("cannot open log file {}: {e}", log_path.display()))
        })?;
    let log_err = log
        .try_clone()
        .map_err(|e| ADBCliError::from(format!("cannot dup log handle: {e}")))?;

    let exe = std::env::current_exe()
        .map_err(|e| ADBCliError::from(format!("cannot resolve own exe: {e}")))?;

    let mut cmd = Command::new(exe);
    cmd.arg("server")
        .arg("start")
        .arg("--address")
        .arg(address.to_string())
        .arg("--pid-file")
        .arg(&pid_file)
        .env(DAEMON_ENV, "1")
        .stdin(Stdio::null())
        .stdout(Stdio::from(log))
        .stderr(Stdio::from(log_err));

    // Detach into a new session so the daemon outlives the controlling terminal.
    // `tokio::process::Command` exposes `pre_exec` inherently (no extra trait).
    #[cfg(unix)]
    {
        // SAFETY: `setsid` is async-signal-safe and the only call made in the
        // child between fork and exec; it creates a new session and has no
        // memory-safety implications.
        unsafe {
            cmd.pre_exec(|| {
                libc::setsid();
                Ok(())
            });
        }
    }

    let child = cmd
        .spawn()
        .map_err(|e| ADBCliError::from(format!("cannot spawn daemon: {e}")))?;
    let child_pid = child
        .id()
        .ok_or_else(|| ADBCliError::from("spawned daemon has no pid".to_string()))?;

    // Record the child's PID for `server kill`. The child also writes it, but
    // doing it here avoids a race where `kill` runs before the child starts.
    write_pid_file(&pid_file, child_pid)
        .map_err(|e| ADBCliError::from(format!("cannot write pid file: {e}")))?;

    tracing::info!(
        "adboost server started (pid {child_pid}) on {address}; logs at {}",
        log_path.display()
    );
    Ok(())
}

/// Build the backend + frontend and serve until SIGTERM (Unix) / the listener
/// errors. `foreground` only affects logging context.
async fn run_server(
    address: SocketAddr,
    _pid_file: &std::path::Path,
    foreground: bool,
) -> ADBCliResult<()> {
    tracing::info!(
        "adboost server {} on {address}",
        if foreground {
            "running (foreground)"
        } else {
            "daemon serving"
        }
    );
    let backend = Arc::new(DefaultDeviceBackend::new());
    // Keep a handle to the backend so we can gracefully close its cached device
    // connections on shutdown (the frontend takes ownership of its own clone).
    let shutdown_backend = Arc::clone(&backend);
    let frontend = AdbServerFrontend::builder(backend).addr(address).build();

    #[cfg(unix)]
    {
        let mut sigterm = tokio::signal::unix::signal(tokio::signal::unix::SignalKind::terminate())
            .map_err(|e| ADBCliError::from(format!("cannot install SIGTERM handler: {e}")))?;
        tokio::select! {
            r = frontend.serve() => r.map_err(|e| ADBCliError::from(format!("server error: {e}")))?,
            _ = sigterm.recv() => tracing::info!("SIGTERM received; shutting down adb server"),
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupt received; shutting down adb server"),
        }
    }
    #[cfg(not(unix))]
    {
        tokio::select! {
            r = frontend.serve() => r.map_err(|e| ADBCliError::from(format!("server error: {e}")))?,
            _ = tokio::signal::ctrl_c() => tracing::info!("interrupt received; shutting down adb server"),
        }
    }
    // Flush a connection-level CLSE to every cached device while the writer tasks
    // are still alive, so devices are not left with orphaned streams.
    shutdown_backend.shutdown().await;
    Ok(())
}

/// `server kill`: stop a running adboost ADB server via its PID file.
pub fn kill(pid_file: Option<PathBuf>) -> ADBCliResult<()> {
    let pid_file = resolve_pid_file(pid_file);
    let Some(pid) = running_pid(&pid_file) else {
        tracing::info!("no running adboost server found (no live pid file)");
        // Clean up a stale file if present.
        let _ = std::fs::remove_file(&pid_file);
        return Ok(());
    };

    #[cfg(unix)]
    {
        let signed_pid = libc::pid_t::try_from(pid)
            .map_err(|_| ADBCliError::from(format!("pid {pid} out of range")))?;
        // SAFETY: `kill` delivers a signal to an existing pid; no memory safety
        // implications. SIGTERM first (graceful), escalate to SIGKILL if needed.
        unsafe {
            if libc::kill(signed_pid, libc::SIGTERM) != 0 {
                let err = std::io::Error::last_os_error();
                return Err(ADBCliError::from(format!(
                    "failed to signal pid {pid}: {err}"
                )));
            }
        }
        // Give it a moment to exit gracefully, then escalate.
        for _ in 0..20 {
            if !is_alive(pid) {
                break;
            }
            std::thread::sleep(std::time::Duration::from_millis(50));
        }
        if is_alive(pid) {
            tracing::warn!("pid {pid} did not exit on SIGTERM; sending SIGKILL");
            // SAFETY: same as above.
            unsafe {
                libc::kill(signed_pid, libc::SIGKILL);
            }
        }
    }
    #[cfg(not(unix))]
    {
        return Err(ADBCliError::from(
            "server kill is only supported on Unix platforms".to_string(),
        ));
    }

    let _ = std::fs::remove_file(&pid_file);
    tracing::info!("stopped adboost server (pid {pid})");
    Ok(())
}
