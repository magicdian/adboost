use adboost::proxy::ADBProxyDevice;
use tokio::io::AsyncWrite;

use crate::models::{ADBCliResult, ForwardCommand, LocalDeviceCommand, ReverseCommand};

pub async fn handle_local_commands(
    mut device: ADBProxyDevice,
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
            ForwardCommand::Add { local, remote } => {
                // Library `forward(remote, local)` is remote-first (like the
                // reverse arm below), while the CLI surface `forward LOCAL REMOTE`
                // is local-first (matches native adb). `forward_library_args` maps
                // between the two orders; do NOT inline it back to a positional
                // call, or a future edit could silently re-swap the ports.
                let (arg0, arg1) = forward_library_args(local, remote);
                Ok(device.forward(arg0, arg1).await?)
            }
        },
        LocalDeviceCommand::Reverse(reverse_command) => match reverse_command {
            ReverseCommand::RemoveAll => Ok(device.reverse_remove_all().await?),
            ReverseCommand::Remove { local } => Ok(device.reverse_remove(local).await?),
            ReverseCommand::Add { remote, local } => Ok(device.reverse(remote, local).await?),
        },
    }
}

/// Map the CLI's local-first `forward LOCAL REMOTE` operands onto the positional
/// arguments expected by the library's remote-first
/// [`ADBProxyDevice::forward`](adboost::proxy::ADBProxyDevice::forward)
/// (`forward(remote, local)`). Returns `(remote, local)`.
///
/// This exists so the CLI↔library order contract is locked by a pure unit test
/// (the real handler needs a live device, so the call site itself is not unit
/// testable). Getting this backwards emits `host:forward:<remote>;<local>` —
/// the exact swap this task fixed.
fn forward_library_args(local: String, remote: String) -> (String, String) {
    (remote, local)
}

#[cfg(test)]
mod tests {
    use super::forward_library_args;

    // Asymmetric ports so a local/remote swap is caught. The CLI parses
    // `forward tcp:1111 tcp:2222` as local=tcp:1111, remote=tcp:2222; the library
    // is remote-first, so the call must be forward(remote=tcp:2222, local=tcp:1111)
    // → emits host:forward:tcp:1111;tcp:2222 (local-first on the wire).
    #[test]
    fn forward_passes_remote_then_local_to_library() {
        let local = "tcp:1111".to_string();
        let remote = "tcp:2222".to_string();
        let (arg0, arg1) = forward_library_args(local.clone(), remote.clone());
        assert_eq!(
            arg0, remote,
            "library forward() is remote-first: first arg must be the CLI's REMOTE"
        );
        assert_eq!(
            arg1, local,
            "library forward() is remote-first: second arg must be the CLI's LOCAL"
        );
    }
}
