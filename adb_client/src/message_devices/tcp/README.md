# Examples

## Get a shell from device

```rust no_run
use std::net::IpAddr;
use adb_client::{tcp::ADBTcpDevice, ADBDeviceExt};
use tokio::io::{empty, sink};

# async fn run() {
let mut device = ADBTcpDevice::new((IpAddr::from([192, 168, 0, 10]), 43210))
    .await
    .expect("cannot find device");
device
    .shell(&mut empty(), Box::pin(sink()))
    .await
    .expect("shell failed");
# }
```
