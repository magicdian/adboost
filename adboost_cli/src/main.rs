#![doc = include_str!("../README.md")]

#[cfg(any(target_os = "linux", target_os = "macos"))]
mod adb_termios;

mod daemon;
mod handlers;
mod models;
mod selftest;
mod utils;

use adb_client::ADBDeviceExt;
use adb_client::mdns::MDNSDiscoveryService;
use adb_client::proxy::ADBProxyServer;
use adb_client::proxy::ADBProxyDevice;
use adb_client::tcp::ADBTcpDevice;
use adb_client::usb::{ADBDeviceInfo, ADBUSBDevice, find_all_connected_adb_devices};

#[cfg(any(target_os = "linux", target_os = "macos"))]
use adb_termios::ADBTermios;

use clap::Parser;
use handlers::{
    handle_emulator_commands, handle_host_commands, handle_local_commands,
    handle_persistent_command,
};
use models::{DeviceCommands, LocalCommand, MainCommand, Opts, ServerCommand};
use std::collections::HashMap;
use std::io::{Write, stdout};
use std::process::ExitCode;
use tabwriter::TabWriter;
use tokio::io::AsyncWriteExt;
use utils::setup_logger;

use crate::models::{ADBCliError, ADBCliResult};

/// Run a [`DeviceCommands`] against any concrete [`ADBDeviceExt`] device.
///
/// `ADBDeviceExt` is now an async trait (AFIT + `trait_variant`) and is no
/// longer `dyn`-compatible — the previous `Box<dyn ADBDeviceExt>` funnel is
/// gone. Each `main` match arm therefore calls this generic, monomorphized
/// function directly with its concrete device type (usb / tcp / server).
async fn run_command<D: ADBDeviceExt>(mut device: D, command: DeviceCommands) -> ADBCliResult<()> {
    match command {
        DeviceCommands::Shell { commands } => {
            if commands.is_empty() {
                // Need to duplicate some code here as ADBTermios [Drop] implementation resets terminal state.
                // Using a scope here would call drop() too early..
                #[cfg(any(target_os = "linux", target_os = "macos"))]
                {
                    let adb_termios = ADBTermios::new(&std::io::stdin())?;
                    adb_termios.set_adb_termios()?;
                    device
                        .shell(&mut tokio::io::stdin(), Box::pin(tokio::io::stdout()))
                        .await?;
                }

                #[cfg(not(any(target_os = "linux", target_os = "macos")))]
                {
                    device
                        .shell(&mut tokio::io::stdin(), Box::pin(tokio::io::stdout()))
                        .await?;
                }
            } else {
                device
                    .shell_command(&commands.join(" "), Some(&mut tokio::io::stdout()), None)
                    .await?;
            }
        }
        DeviceCommands::Pull {
            source,
            destination,
        } => {
            let mut output = tokio::fs::File::create(&destination).await?;
            device.pull(&source, &mut output).await?;
            tracing::info!("Downloaded {source} as {destination}");
        }
        DeviceCommands::Stat { path } => {
            let stat_response = device.stat(&path).await?;
            println!("{stat_response}");
        }
        DeviceCommands::StatExtended { path } => {
            let stat_response = device.stat_extended(&path).await?;
            if let Some(stat_response) = stat_response {
                println!("{stat_response}");
            } else {
                println!("No such file or directory");
            }
        }
        DeviceCommands::Reboot { reboot_type } => {
            tracing::info!("Reboots device in mode {reboot_type:?}");
            device.reboot(reboot_type.into()).await?;
        }
        DeviceCommands::Push { filename, path } => {
            let mut input = tokio::fs::File::open(&filename).await?;
            device.push(&mut input, &path).await?;
            tracing::info!("Uploaded {filename} to {path}");
        }
        DeviceCommands::Root => {
            device.root().await?;
            tracing::info!("Restarted adbd as root");
        }
        DeviceCommands::Run { package, activity } => {
            let output = device.run_activity(&package, &activity).await?;
            let mut out = tokio::io::stdout();
            out.write_all(&output).await?;
            out.flush().await?;
        }
        DeviceCommands::Install { path, user } => {
            tracing::info!("Starting installation of APK {}...", path.display());
            device.install(&path, user.as_deref()).await?;
        }
        DeviceCommands::Uninstall { package, user } => {
            tracing::info!("Uninstalling the package {package}...");
            device.uninstall(&package, user.as_deref()).await?;
        }
        DeviceCommands::Framebuffer { path } => {
            device.framebuffer(&path).await?;
            tracing::info!("Successfully dumped framebuffer at path {path}");
        }
        DeviceCommands::List { path } => {
            let dirs = device.list(&path).await?;
            for dir in dirs {
                tracing::info!("{dir}");
            }
        }
    }

    Ok(())
}

#[tokio::main]
async fn main() -> ExitCode {
    if let Err(err) = inner_main().await {
        tracing::error!("{err}");
        return ExitCode::FAILURE;
    }

    ExitCode::SUCCESS
}

async fn inner_main() -> ADBCliResult<()> {
    // This depends on `clap`
    let opts = Opts::parse();

    setup_logger(opts.debug);

    // Directly handling methods / commands that aren't linked to the
    // [`ADBDeviceExt`] trait. The device-bearing arms build a concrete device
    // and dispatch through the generic, async `run_command` (the trait is no
    // longer `dyn`-compatible, so there is no shared `Box<dyn ADBDeviceExt>`).
    match opts.command {
        MainCommand::Host(server_command) => Ok(handle_host_commands(server_command).await?),
        MainCommand::Emu(emulator_command) => handle_emulator_commands(emulator_command).await,
        MainCommand::Local(server_command) => {
            // Must start server to communicate with device, but only if this is a local one.
            let server_address_ip = server_command.address.ip();
            if server_address_ip.is_loopback() || server_address_ip.is_unspecified() {
                ADBProxyServer::start(&HashMap::default(), &None);
            }

            let device = if let Some(id) = server_command.transport_id {
                ADBProxyDevice::new_with_transport_id(id, Some(server_command.address))
            } else if let Some(serial) = server_command.serial {
                ADBProxyDevice::new(serial, Some(server_command.address))
            } else {
                ADBProxyDevice::autodetect(Some(server_command.address))
            };

            match server_command.command {
                LocalCommand::DeviceCommands(device_commands) => {
                    run_command(device, device_commands).await
                }
                LocalCommand::LocalDeviceCommand(local_device_command) => {
                    handle_local_commands(device, local_device_command).await
                }
            }
        }
        MainCommand::Server { command } => match command {
            ServerCommand::Start {
                address,
                foreground,
                pid_file,
                log_file,
            } => daemon::start(address, foreground, pid_file, log_file).await,
            ServerCommand::Kill { pid_file } => daemon::kill(pid_file),
        },
        MainCommand::Usb(usb_command) => handle_usb_command(usb_command).await,
        MainCommand::Tcp(tcp_command) => {
            let device = match tcp_command.path_to_private_key {
                Some(pk) => {
                    ADBTcpDevice::new_with_custom_private_key(tcp_command.address, pk).await?
                }
                None => ADBTcpDevice::new(tcp_command.address).await?,
            };
            run_command(device, tcp_command.commands).await
        }
        MainCommand::Persistent(persistent_command) => {
            handle_persistent_command(persistent_command).await
        }
        MainCommand::Mdns => handle_mdns_command(),
        MainCommand::Selftest(selftest_command) => selftest::run(selftest_command).await,
        MainCommand::Version => {
            println!("{} {}", env!("CARGO_PKG_NAME"), utils::long_version());
            Ok(())
        }
    }
}

async fn handle_usb_command(usb_command: models::UsbCommand) -> ADBCliResult<()> {
    if usb_command.list_devices {
        let devices = find_all_connected_adb_devices()?;

        let mut writer = TabWriter::new(stdout()).alignment(tabwriter::Alignment::Center);
        writeln!(writer, "Index\tVendor ID\tProduct ID\tSerial\tDevice Description")?;
        writeln!(writer, "-----\t---------\t----------\t------\t----------------")?;

        for (
            index,
            ADBDeviceInfo {
                vendor_id,
                product_id,
                device_description,
                serial,
            },
        ) in devices.iter().enumerate()
        {
            let serial = serial.as_deref().unwrap_or("-");
            writeln!(
                writer,
                "#{index}\t{vendor_id:04x}\t{product_id:04x}\t{serial}\t{device_description}",
            )?;
        }

        writer.flush()?;

        return Ok(());
    }

    let device = match (usb_command.vendor_id, usb_command.product_id) {
        (Some(vid), Some(pid)) => match usb_command.path_to_private_key {
            Some(pk) => ADBUSBDevice::new_with_custom_private_key(vid, pid, pk).await?,
            None => ADBUSBDevice::new(vid, pid).await?,
        },
        (None, None) => match usb_command.path_to_private_key {
            Some(pk) => ADBUSBDevice::autodetect_with_custom_private_key(pk).await?,
            None => ADBUSBDevice::autodetect().await?,
        },
        _ => {
            return Err(ADBCliError::Standard(
                "cannot specify flags --vendor-id without --product-id or vice versa".into(),
            ));
        }
    };

    if let Some(command) = usb_command.commands {
        run_command(device, command).await
    } else {
        Err(ADBCliError::Standard("no command specified".into()))
    }
}

fn handle_mdns_command() -> ADBCliResult<()> {
    let mut service = MDNSDiscoveryService::new()?;

    let (tx, rx) = std::sync::mpsc::channel();
    service.start(tx)?;

    tracing::info!("Starting mdns discovery...");
    while let Ok(device) = rx.recv() {
        tracing::info!(
            "Found device fullname='{}' with ipv4 addresses={:?} and ipv6 addresses={:?}",
            device.fullname,
            device.ipv4_addresses(),
            device.ipv6_addresses()
        );
    }

    Ok(service.shutdown()?)
}
