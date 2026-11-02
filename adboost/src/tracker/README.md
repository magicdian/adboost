# ADB device tracker

`adboost::tracker` owns the adb **device-tracker** wire family on the client
side. It replaces downstream text parsing with typed, complete snapshots:

- `host:host-features` negotiation;
- `host:track-devices-proto-binary` when the server advertises
  `devicetracker_proto_format`;
- `host:track-devices-l` otherwise, including AOSP's space-padded rows and
  adboost's own tab-padded frontend;
- one length-prefixed frame = one [`DeviceSnapshot`];
- validated [`DeviceSerial`] identities, so a raw row can never become a
  device;
- cancel-safe [`DeviceTracker::next_snapshot`] and an explicit
  [`ReconnectPolicy`] with a [`TrackerHealth`] watch channel.

The existing `ADBProxyServer::track_devices` callback API is unchanged; the
tracker is the snapshot-oriented alternative.

## Snapshot loop

```rust no_run
# async fn run() -> Result<(), adboost::tracker::TrackerError> {
use adboost::tracker::DeviceTracker;

let mut tracker = DeviceTracker::connect_default().await?;
while let Some(snapshot) = tracker.next_snapshot().await? {
    for device in snapshot.devices {
        println!("{} {} ({})", device.serial, device.state, device.transport);
    }
}
# Ok(())
# }
```

## Exact format or no reconnect

```rust no_run
# async fn run() -> Result<(), adboost::tracker::TrackerError> {
use adboost::tracker::{DeviceTracker, TrackerConfig, TrackerFormat};

let config = TrackerConfig {
    format: Some(TrackerFormat::LongText),
    reconnect: None,
    ..TrackerConfig::default()
};
let mut tracker = DeviceTracker::connect_with_config(config).await?;
let snapshot = tracker.next_snapshot().await?;
# let _ = snapshot;
# Ok(())
# }
```

[`DeviceSnapshot`]: crate::tracker::DeviceSnapshot
[`DeviceSerial`]: crate::tracker::DeviceSerial
[`DeviceTracker::next_snapshot`]: crate::tracker::DeviceTracker::next_snapshot
[`ReconnectPolicy`]: crate::tracker::ReconnectPolicy
[`TrackerHealth`]: crate::tracker::TrackerHealth
