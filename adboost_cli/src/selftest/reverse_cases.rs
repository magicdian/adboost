//! Reverse data-plane self-test cases (through-server channel).
//!
//! These validate end-to-end reverse tunneling: the host binds a target server,
//! a reverse rule is set through adboost's server (`ADBProxyDevice::reverse`),
//! and a device-side client connects to the device-listen port — traffic must
//! tunnel back to the host target.
//!
//! - [`reverse_echo`] (always): a tiny host echo server + device `nc`/toybox
//!   client. Basic connectivity.
//! - [`reverse_iperf3`] (auto): only when the device has `iperf3`; measures
//!   throughput over the reverse tunnel (also a USB-link bandwidth datapoint).

use std::time::Duration;

use adboost::ADBDeviceExt;
use adboost::proxy::ADBProxyDevice;
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;

use super::report::Outcome;

/// Shell out on the device and capture stdout (lossy UTF-8).
async fn device_shell(device: &mut ADBProxyDevice, cmd: &str) -> Result<String, String> {
    let mut out = Vec::new();
    device
        .shell_command(&cmd, Some(&mut out), None)
        .await
        .map_err(|e| format!("device shell `{cmd}` failed: {e}"))?;
    Ok(String::from_utf8_lossy(&out).into_owned())
}

/// Whether the device has a usable `nc` (netcat) — needed for the echo case.
pub async fn device_has_nc(device: &mut ADBProxyDevice) -> bool {
    device_shell(device, "which nc || command -v nc")
        .await
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Whether the device has `iperf3`.
pub async fn device_has_iperf3(device: &mut ADBProxyDevice) -> bool {
    device_shell(device, "which iperf3 || command -v iperf3")
        .await
        .map(|s| !s.trim().is_empty())
        .unwrap_or(false)
}

/// Basic reverse connectivity: host echo server ← device `nc` client through the
/// reverse tunnel.
///
/// `device_port` is the port the device listens on (the reverse rule's remote);
/// the host target is the ephemeral port the echo server binds.
pub async fn reverse_echo(device: &mut ADBProxyDevice, device_port: u16) -> Outcome {
    const MARKER: &str = "adboost_reverse_echo_5c1a";

    // 1) Host echo server on an ephemeral port. One connection, echo, close.
    let listener = match TcpListener::bind(("127.0.0.1", 0)).await {
        Ok(l) => l,
        Err(e) => return Outcome::Failed(format!("host echo bind failed: {e}")),
    };
    let host_port = match listener.local_addr() {
        Ok(a) => a.port(),
        Err(e) => return Outcome::Failed(format!("host echo addr failed: {e}")),
    };
    let echo = tokio::spawn(async move {
        if let Ok((mut sock, _)) = listener.accept().await {
            let mut buf = vec![0u8; 1024];
            if let Ok(n) = sock.read(&mut buf).await {
                let _ = sock.write_all(&buf[..n]).await;
                let _ = sock.flush().await;
            }
        }
    });

    // 2) Set the reverse rule: device:<device_port> → host:<host_port>.
    if let Err(e) = device
        .reverse(format!("tcp:{device_port}"), format!("tcp:{host_port}"))
        .await
    {
        echo.abort();
        return Outcome::Failed(format!("reverse setup failed: {e}"));
    }

    // 3) Device connects to its own reversed port and sends the marker.
    //    The reply (echoed marker) comes back on the SAME connection, so the
    //    client must keep its send side open long enough to read it. A bare
    //    `echo <m> | nc ...` closes stdin immediately; toybox `nc` then
    //    half-closes (and on some builds tears the whole connection down)
    //    BEFORE the echo round-trips — the reply is lost, even through a real
    //    `adb` server (device-verified). Holding stdin open with a short
    //    `sleep` keeps the connection alive for the reply.
    let cmd = format!("(echo {MARKER}; sleep 2) | nc 127.0.0.1 {device_port}");
    let result = tokio::time::timeout(Duration::from_secs(10), device_shell(device, &cmd)).await;

    // 4) Teardown the rule (best-effort).
    let _ = device.reverse_remove(format!("tcp:{device_port}")).await;
    echo.abort();

    match result {
        Ok(Ok(out)) if out.contains(MARKER) => Outcome::Passed,
        Ok(Ok(out)) => Outcome::Failed(format!("echo via reverse returned {out:?}")),
        Ok(Err(e)) => Outcome::Failed(e),
        Err(_) => Outcome::Failed("reverse echo timed out (10s)".into()),
    }
}

/// Reverse throughput: host `iperf3 -s` ← device `iperf3 -c` through the tunnel.
/// Returns the measured summary line on success.
pub async fn reverse_iperf3(device: &mut ADBProxyDevice, device_port: u16) -> Outcome {
    // Host iperf3 server on an ephemeral port (one-off, JSON for easy parse).
    let host_port = match pick_free_port().await {
        Ok(p) => p,
        Err(e) => return Outcome::Failed(e),
    };
    let mut server = match tokio::process::Command::new("iperf3")
        .args(["-s", "-p", &host_port.to_string(), "-1"]) // -1: exit after one client
        .kill_on_drop(true)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()
    {
        Ok(c) => c,
        Err(e) => return Outcome::Skipped(format!("host iperf3 unavailable: {e}")),
    };
    // Give the server a moment to bind.
    tokio::time::sleep(Duration::from_millis(300)).await;

    if let Err(e) = device
        .reverse(format!("tcp:{device_port}"), format!("tcp:{host_port}"))
        .await
    {
        let _ = server.kill().await;
        return Outcome::Failed(format!("reverse setup failed: {e}"));
    }

    // Device runs a short (3s) iperf3 client to its reversed port.
    let cmd = format!("iperf3 -c 127.0.0.1 -p {device_port} -t 3");
    let result = tokio::time::timeout(Duration::from_secs(20), device_shell(device, &cmd)).await;

    let _ = device.reverse_remove(format!("tcp:{device_port}")).await;
    let _ = server.kill().await;

    match result {
        Ok(Ok(out)) if out.contains("receiver") || out.contains("sender") => {
            // Surface the throughput summary lines for the operator.
            let summary: String = out
                .lines()
                .filter(|l| l.contains("sender") || l.contains("receiver"))
                .collect::<Vec<_>>()
                .join(" | ");
            tracing::info!("reverse iperf3: {summary}");
            Outcome::Passed
        }
        Ok(Ok(out)) => Outcome::Failed(format!("iperf3 via reverse gave no result: {out:?}")),
        Ok(Err(e)) => Outcome::Failed(e),
        Err(_) => Outcome::Failed("reverse iperf3 timed out (20s)".into()),
    }
}

/// Bind+drop an ephemeral TCP port to discover a free port number for iperf3
/// (which needs to bind it itself).
async fn pick_free_port() -> Result<u16, String> {
    let l = TcpListener::bind(("127.0.0.1", 0))
        .await
        .map_err(|e| format!("cannot pick free port: {e}"))?;
    l.local_addr()
        .map(|a| a.port())
        .map_err(|e| format!("cannot read free port: {e}"))
}
