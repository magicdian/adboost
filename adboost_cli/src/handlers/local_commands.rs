use adb_client::server_device::ADBServerDevice;
use tokio::io::AsyncWrite;

use crate::models::{ADBCliResult, ForwardCommand, LocalDeviceCommand, ReverseCommand};

pub async fn handle_local_commands(
    mut device: ADBServerDevice,
    local_device_commands: LocalDeviceCommand,
) -> ADBCliResult<()> {
    match local_device_commands {
        LocalDeviceCommand::HostFeatures => {
            let features = device
                .host_features()
                .await?
                .iter()
                .map(ToString::to_string)
                .reduce(|a, b| format!("{a},{b}"))
                .unwrap_or_default();
            tracing::info!("Available host features: {features}");

            Ok(())
        }
        LocalDeviceCommand::Logcat { path } => {
            let writer: Box<dyn AsyncWrite + Unpin + Send> = if let Some(path) = path {
                let log_file = tokio::fs::File::create(path).await?;
                Box::new(log_file)
            } else {
                Box::new(tokio::io::stdout())
            };
            Ok(device.get_logs(writer).await?)
        }
        LocalDeviceCommand::Forward(forward_command) => match forward_command {
            ForwardCommand::RemoveAll => Ok(device.forward_remove_all().await?),
            ForwardCommand::Remove { local } => Ok(device.forward_remove(local).await?),
            ForwardCommand::Add { local, remote } => Ok(device.forward(local, remote).await?),
        },
        LocalDeviceCommand::Reverse(reverse_command) => match reverse_command {
            ReverseCommand::RemoveAll => Ok(device.reverse_remove_all().await?),
            ReverseCommand::Remove { local } => Ok(device.reverse_remove(local).await?),
            ReverseCommand::Add { remote, local } => Ok(device.reverse(remote, local).await?),
        },
    }
}
