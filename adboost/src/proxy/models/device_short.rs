use std::fmt::Display;

use crate::tracker::wire::parse_line;
use crate::{RustADBError, proxy::DeviceState};

/// Represents a device connected to the ADB server.
#[derive(Debug, Clone)]
pub struct DeviceShort {
    /// Unique device identifier.
    pub identifier: String,
    /// Connection state of the device.
    pub state: DeviceState,
}

impl Display for DeviceShort {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        write!(f, "{}\t{}", self.identifier, self.state)
    }
}

impl TryFrom<Vec<u8>> for DeviceShort {
    type Error = RustADBError;

    fn try_from(value: Vec<u8>) -> Result<Self, Self::Error> {
        let device = parse_line(&value).map_err(|()| RustADBError::RegexParsingError)?;
        Ok(Self {
            identifier: device.serial.to_string(),
            state: DeviceState::try_from(device.state)?,
        })
    }
}
