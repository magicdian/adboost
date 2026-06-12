# Examples

This module is the **proxy client**: it connects to (and proxies commands
through) an external ADB server daemon listening on TCP `:5037`. It is *not*
adboost's own ADB server — for that, see the [`crate::server`] module.

## Get available ADB devices

```rust no_run
use adb_client::proxy::ADBProxyServer;
use std::net::{SocketAddrV4, Ipv4Addr};

// A custom server address can be provided
let server_ip = Ipv4Addr::new(127, 0, 0, 1);
let server_port = 5037;

let mut server = ADBProxyServer::new(SocketAddrV4::new(server_ip, server_port));
server.devices();
```

## Launch a command on device

```rust no_run
use adb_client::{proxy::ADBProxyServer, ADBDeviceExt};

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
use adb_client::proxy::ADBProxyServer;

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
