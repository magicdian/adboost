use adboost::emulator::ADBEmulatorDevice;

use crate::models::{ADBCliResult, EmuCommand, EmulatorCommand};

pub async fn handle_emulator_commands(emulator_command: EmulatorCommand) -> ADBCliResult<()> {
    let mut emulator = ADBEmulatorDevice::new(emulator_command.serial, None)?;

    match emulator_command.command {
        EmuCommand::Sms {
            phone_number,
            content,
        } => {
            emulator.send_sms(&phone_number, &content).await?;
            tracing::info!("SMS sent to {phone_number}");
        }
        EmuCommand::Rotate => emulator.rotate().await?,
        EmuCommand::AvdDiscoveryPath => {
            let path = emulator.avd_discovery_path().await?;
            tracing::info!("AVD discovery path: {}", path.display());
            println!("{}", path.display());
        }
        EmuCommand::AvdGrpcPort => {
            let port = emulator.avd_grpc_port().await?;
            tracing::info!("gRPC port: {port}");
            println!("{port}");
        }
        EmuCommand::Raw { command } => {
            let response = emulator.send_raw_command(&command).await?;
            println!("{response}");
        }
    }

    Ok(())
}
