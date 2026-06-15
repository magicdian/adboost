use std::path::PathBuf;

use crate::{
    Result,
    emulator::{ADBEmulatorCommand, ADBEmulatorDevice},
};

impl ADBEmulatorDevice {
    /// Get the AVD discovery path of this emulator
    pub async fn avd_discovery_path(&mut self) -> Result<PathBuf> {
        let path = self
            .connect()
            .await?
            .send_command(&ADBEmulatorCommand::AvdDiscoveryPath)
            .await?;
        Ok(PathBuf::from(path.trim()))
    }
    /// Get the gRPC port of this emulator
    pub async fn avd_grpc_port(&mut self) -> Result<u16> {
        let port = self
            .connect()
            .await?
            .send_command(&ADBEmulatorCommand::AvdGrpcPort)
            .await?;
        Ok(port.trim().parse()?)
    }
}
