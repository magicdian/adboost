# Examples

This module is the **proxy client**: it connects to (and proxies commands
through) an external ADB server daemon listening on TCP `:5037`. It is *not*
adboost's own ADB server — for that, see the [`crate::server`] module.

## Get available ADB devices

```rust no_run
use adboost::proxy::ADBProxyServer;
use std::net::{SocketAddrV4, Ipv4Addr};

// A custom server address can be provided
let server_ip = Ipv4Addr::new(127, 0, 0, 1);
let server_port = 5037;

let mut server = ADBProxyServer::new(SocketAddrV4::new(server_ip, server_port));
server.devices();
```

## Launch a command on device

```rust no_run
use adboost::{proxy::ADBProxyServer, ADBDeviceExt};

# async fn run() {
let mut server = ADBProxyServer::default();
let mut device = server.get_device().await.expect("cannot get device");
let mut output = Vec::new();
device
    .shell_command(&"df -h", Some(&mut output), None)
    .await
    .expect("shell command failed");
# }
```

## Push a file to the device

```rust no_run
use adboost::proxy::ADBProxyServer;

# async fn run() {
let mut server = ADBProxyServer::default();
let mut device = server.get_device().await.expect("cannot get device");
let mut input = tokio::fs::File::open("/tmp/file.txt")
    .await
    .expect("Cannot open file");
device
    .push(&mut input, &"/data/local/tmp")
    .await
    .expect("push failed");
# }
```

## Device operations: install, uninstall, root, remount

The typed operations are the supported path for anything that needs flags,
structured results, progress, or cancellation. Each returns a typed outcome (or a
typed error) instead of a bare string.

```rust no_run
use adboost::{InstallControl, InstallOptions, RootState};
use adboost::proxy::ADBProxyServer;
use tokio::sync::mpsc;
use tokio_util::sync::CancellationToken;

# async fn run() {
let mut server = ADBProxyServer::default();
let mut device = server.get_device().await.expect("cannot get device");

// Upgrade install with granted permissions, progress, and cancellation.
let (progress_tx, mut progress_rx) = mpsc::channel(8);
let cancel = CancellationToken::new();
let options = InstallOptions {
    replace: true,
    grant: true,
    ..InstallOptions::default()
};
let control = InstallControl {
    progress: Some(progress_tx),
    cancel: Some(cancel.clone()),
    ..InstallControl::default()
};
device
    .install_with_options("/tmp/app.apk", &options, control)
    .await
    .expect("install failed");

// The receiver observes monotonically increasing progress; a full channel drops
// samples and never blocks the transfer.
while let Some(sample) = progress_rx.recv().await {
    println!("{} / {}", sample.bytes_sent, sample.total_bytes);
}

// Root is only "successful" if the device says so.
let root = device.root_with_outcome().await.expect("transport error");
match root.state {
    RootState::Restarting => { /* wait (bounded) for the device to reconnect */ }
    RootState::AlreadyRoot => {}
    RootState::Refused => panic!("production build refused root: {}", root.raw),
    RootState::Unknown => println!("unrecognized receipt: {}", root.raw),
}

let uninstall = device
    .uninstall_with_outcome("com.example.app", None)
    .await
    .expect("uninstall failed");
assert!(uninstall.success);
# }
```

### Semantics

- **Install reply reading** accumulates the device reply to EOF (capped at 64 KiB)
  and classifies the last `Success` / `Failure [REASON]` line, so fragmented
  replies and warning noise are handled correctly.
- **Progress** is an observation surface: samples are sent non-blocking and a full
  channel drops a sample rather than stalling the APK transfer.
- **Cancellation** (`InstallControl::cancel`) stops the local transfer and closes
  the connection, so the device's install observes EOF. This is **best-effort**:
  the device may already have finished installing before the abort lands.
- **Root/unroot** return the classified receipt and never wait for the device to
  reconnect — that bounded wait is caller policy. The legacy `root()` errors with
  `RootRefused` when the device refuses.
- **Remount** reads the full reply and accepts vendor wording: success when the
  device reports `remount succeeded` or at least one `Using …` line, and no
  failure marker is present. The raw output is always available on
  `RemountOutcome::raw`.

### Relationship to `exec`

`exec` remains a raw, untyped byte stream: no exit code, no progress, no
cancellation, and no reply parsing. When an operation needs structured results,
progress, or cancellation, use the typed methods above rather than re-implementing
the `cmd package …` protocol on top of `exec` — two implementations drift.

