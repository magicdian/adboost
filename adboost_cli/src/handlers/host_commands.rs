use adb_client::{
    Result,
    proxy::{ADBProxyServer, DeviceShort, MDNSBackend, WaitForDeviceState},
};

use crate::models::{HostCommand, MdnsCommand, ProxyCommand};

pub async fn handle_host_commands(server_command: ProxyCommand<HostCommand>) -> Result<()> {
    let mut adb_server = ADBProxyServer::new(server_command.address);

    match server_command.command {
        HostCommand::Version => {
            let version = adb_server.version().await?;
            tracing::info!("Android Debug Bridge version {version}");
            tracing::info!("Package version {}-rust", std::env!("CARGO_PKG_VERSION"));
        }
        HostCommand::Kill => {
            adb_server.kill().await?;
        }
        HostCommand::Devices { long } => {
            if long {
                tracing::info!("List of devices attached (extended)");
                for device in adb_server.devices_long().await? {
                    tracing::info!("{device}");
                }
            } else {
                tracing::info!("List of devices attached");
                for device in adb_server.devices().await? {
                    tracing::info!("{device}");
                }
            }
        }
        HostCommand::TrackDevices => {
            let callback = |device: DeviceShort| {
                tracing::info!("{device}");
                Ok(())
            };
            tracing::info!("Live list of devices attached");
            adb_server.track_devices(callback).await?;
        }
        HostCommand::Pair { address, code } => {
            adb_server.pair(address, code).await?;
            tracing::info!("Paired device {address}");
        }
        HostCommand::Connect { address } => {
            adb_server.connect_device(address).await?;
            tracing::info!("Connected to {address}");
        }
        HostCommand::Disconnect { address } => {
            adb_server.disconnect_device(address).await?;
            tracing::info!("Disconnected {address}");
        }
        HostCommand::Mdns { subcommand } => match subcommand {
            MdnsCommand::Check => {
                let check = adb_server.mdns_check().await?;
                let server_status = adb_server.server_status().await?;
                match server_status.mdns_backend {
                    MDNSBackend::Unknown => tracing::info!("unknown mdns backend..."),
                    MDNSBackend::Bonjour => {
                        if check {
                            tracing::info!("mdns daemon version [Bonjour]");
                        } else {
                            tracing::info!("ERROR: mdns daemon unavailable");
                        }
                    }
                    MDNSBackend::OpenScreen => {
                        tracing::info!("mdns daemon version [Openscreen discovery 0.0.0]");
                    }
                }
            }
            MdnsCommand::Services => {
                tracing::info!("List of discovered mdns services");
                for service in adb_server.mdns_services().await? {
                    tracing::info!("{service}");
                }
            }
        },
        HostCommand::ServerStatus => {
            tracing::info!("{}", adb_server.server_status().await?);
        }
        HostCommand::WaitForDevice { transport } => {
            tracing::info!("waiting for device to be connected...");
            adb_server
                .wait_for_device(WaitForDeviceState::Device, transport)
                .await?;
        }
    }

    Ok(())
}
