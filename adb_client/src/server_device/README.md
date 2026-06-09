# Examples

## Launch a command on device

```rust no_run
use adb_client::{server::ADBServer, ADBDeviceExt};

# async fn run() {
let mut server = ADBServer::default();
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
use adb_client::server::ADBServer;

# async fn run() {
let mut server = ADBServer::default();
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
