use nusb::{DeviceInfo, MaybeFuture};

use crate::{Result, RustADBError};

const ADB_SUBCLASS: u8 = 0x42;
const ADB_PROTOCOL: u8 = 0x1;

// Some devices require choosing the file transfer mode
// for usb debugging to take effect.
const BULK_CLASS: u8 = 0xdc;
const BULK_ADB_SUBCLASS: u8 = 2;

const LIBUSB_CLASS_VENDOR_SPEC: u8 = 0xff;

/// Represents an Android device connected via USB
#[derive(Clone, Debug)]
pub struct ADBDeviceInfo {
    /// Vendor ID of the device
    pub vendor_id: u16,
    /// Product ID of the device
    pub product_id: u16,
    /// Textual description of the device
    pub device_description: String,
}

/// Find and return a list of all connected Android devices with known interface class and subclass values
pub fn find_all_connected_adb_devices() -> Result<Vec<ADBDeviceInfo>> {
    let mut found_devices = vec![];

    for device in nusb::list_devices().wait()? {
        if !is_adb_device(&device) {
            continue;
        }

        // `nusb` exposes the manufacturer / product strings on the cached
        // `DeviceInfo` (populated during enumeration), so unlike the previous
        // `rusb` code we do not need to open the device to read them. We still
        // preserve the "Unknown" fallback when a string is unavailable.
        let manufacturer = device.manufacturer_string().unwrap_or("Unknown");
        let product = device.product_string().unwrap_or("Unknown");

        found_devices.push(ADBDeviceInfo {
            vendor_id: device.vendor_id(),
            product_id: device.product_id(),
            device_description: format!("{manufacturer} {product}"),
        });
    }

    Ok(found_devices)
}

/// Find and return an USB-connected Android device with known interface class and subclass values.
///
/// Returns the first device found or None if no device is found.
/// If multiple devices are found, an error is returned.
pub fn get_single_connected_adb_device() -> Result<Option<ADBDeviceInfo>> {
    let found_devices = find_all_connected_adb_devices()?;
    match (found_devices.first(), found_devices.get(1)) {
        (None, _) => Ok(None),
        (Some(device_info), None) => {
            tracing::debug!(
                "Autodetect device {:04x}:{:04x} - {}",
                device_info.vendor_id,
                device_info.product_id,
                device_info.device_description
            );
            Ok(Some(device_info.clone()))
        }
        (Some(device_1), Some(device_2)) => Err(RustADBError::DeviceNotFound(format!(
            "Found two Android devices {:04x}:{:04x} and {:04x}:{:04x}",
            device_1.vendor_id, device_1.product_id, device_2.vendor_id, device_2.product_id
        ))),
    }
}

/// Check whether a device is an ADB device, based on its interface
/// class / subclass / protocol triple.
///
/// `nusb` exposes the per-interface class/subclass/protocol on the cached
/// `DeviceInfo`, so this matches the previous `rusb` logic without opening the
/// device or walking configuration descriptors.
fn is_adb_device(device: &DeviceInfo) -> bool {
    device.interfaces().any(|interface| {
        let proto = interface.protocol();
        let class = interface.class();
        let subcl = interface.subclass();
        proto == ADB_PROTOCOL
            && ((class == LIBUSB_CLASS_VENDOR_SPEC && subcl == ADB_SUBCLASS)
                || (class == BULK_CLASS && subcl == BULK_ADB_SUBCLASS))
    })
}
