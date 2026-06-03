use std::{
    sync::{Arc, Mutex},
    time::Duration,
};

use nusb::{
    DeviceInfo, Interface, MaybeFuture,
    descriptors::TransferType,
    transfer::{Buffer, Bulk, Direction, In, Out, TransferError},
};

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::{ADBTransportMessage, ADBTransportMessageHeader},
        message_commands::MessageCommand,
    },
};

const ADB_CLASS: u8 = 0xFF;
const ADB_SUBCLASS: u8 = 0x42;
const ADB_PROTOCOL: u8 = 0x01;

/// Address + max-packet-size pair for a discovered bulk endpoint.
#[derive(Clone, Copy, Debug)]
struct EndpointInfo {
    iface: u8,
    address: u8,
    max_packet_size: usize,
}

/// Internal connection state shared between clones of a [`USBTransport`].
///
/// `nusb` endpoints are `&mut self`-exclusive and not `Clone`, so the
/// endpoints live behind shared `Mutex`es. Cloning a `USBTransport` shares the
/// *same* underlying handle — this mirrors the previous `rusb` behavior where
/// clones shared an `Arc<DeviceHandle>`, which the reader-thread / writer
/// concurrency model in `ADBMessageDevice` and the persistent connection rely
/// upon.
///
/// The IN (read) and OUT (write) endpoints use **separate** locks so a reader
/// thread blocked in a long `transfer_blocking` on the IN endpoint never
/// blocks a concurrent writer on the OUT endpoint. This preserves the
/// independent reader/writer concurrency the previous `rusb` implementation
/// had with two distinct endpoints sharing one handle.
#[derive(Debug, Default)]
struct Connection {
    interface: Option<Interface>,
    read_endpoint: Option<nusb::Endpoint<Bulk, In>>,
    read_info: Option<EndpointInfo>,
}

#[derive(Debug, Default)]
struct WriteConnection {
    write_endpoint: Option<nusb::Endpoint<Bulk, Out>>,
    write_info: Option<EndpointInfo>,
}

/// Transport running on USB
#[derive(Debug, Clone)]
pub struct USBTransport {
    device_info: DeviceInfo,
    connection: Arc<Mutex<Connection>>,
    write_connection: Arc<Mutex<WriteConnection>>,
}

impl USBTransport {
    /// Instantiate a new [`USBTransport`].
    /// Only the first device with given `vendor_id` and `product_id` is returned.
    pub fn new(vendor_id: u16, product_id: u16) -> Result<Self> {
        for device_info in nusb::list_devices().wait()? {
            if device_info.vendor_id() == vendor_id && device_info.product_id() == product_id {
                return Ok(Self::new_from_device(device_info));
            }
        }

        Err(RustADBError::DeviceNotFound(format!(
            "cannot find USB device with vendor_id={vendor_id} and product_id={product_id}",
        )))
    }

    /// Instantiate a new [`USBTransport`] from a [`nusb::DeviceInfo`].
    ///
    /// Devices can be enumerated using [`nusb::list_devices()`] and then filtered out to get desired device.
    #[must_use]
    pub fn new_from_device(device_info: DeviceInfo) -> Self {
        Self {
            device_info,
            connection: Arc::new(Mutex::new(Connection::default())),
            write_connection: Arc::new(Mutex::new(WriteConnection::default())),
        }
    }

    pub(crate) fn vendor_id(&self) -> u16 {
        self.device_info.vendor_id()
    }

    pub(crate) fn product_id(&self) -> u16 {
        self.device_info.product_id()
    }

    fn read_info(&self) -> Result<EndpointInfo> {
        self.connection
            .lock()?
            .read_info
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no read endpoint setup",
            )))
    }

    fn write_info(&self) -> Result<EndpointInfo> {
        self.write_connection
            .lock()?
            .write_info
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no write endpoint setup",
            )))
    }

    /// Discover the ADB bulk IN / OUT endpoints exposed by the device.
    ///
    /// This uses cached descriptor data (no IO) and mirrors the previous
    /// `rusb`-based `find_endpoints`: it matches the vendor-specific ADB
    /// interface (`class = 0xFF`, `subclass = 0x42`, `protocol = 0x01`) and
    /// returns the first bulk IN / OUT pair found.
    fn find_endpoints(device: &nusb::Device) -> Result<(EndpointInfo, EndpointInfo)> {
        let mut read_endpoint: Option<EndpointInfo> = None;
        let mut write_endpoint: Option<EndpointInfo> = None;

        for config_desc in device.configurations() {
            for interface_group in config_desc.interfaces() {
                for interface_desc in interface_group.alt_settings() {
                    if interface_desc.class() != ADB_CLASS
                        || interface_desc.subclass() != ADB_SUBCLASS
                        || interface_desc.protocol() != ADB_PROTOCOL
                    {
                        continue;
                    }

                    for endpoint_desc in interface_desc.endpoints() {
                        if endpoint_desc.transfer_type() != TransferType::Bulk {
                            continue;
                        }

                        let endpoint = EndpointInfo {
                            iface: interface_desc.interface_number(),
                            address: endpoint_desc.address(),
                            max_packet_size: endpoint_desc.max_packet_size(),
                        };

                        match endpoint_desc.direction() {
                            Direction::In => {
                                if let Some(write_endpoint) = write_endpoint {
                                    return Ok((endpoint, write_endpoint));
                                }
                                read_endpoint = Some(endpoint);
                            }
                            Direction::Out => {
                                if let Some(read_endpoint) = read_endpoint {
                                    return Ok((read_endpoint, endpoint));
                                }
                                write_endpoint = Some(endpoint);
                            }
                        }
                    }
                }
            }
        }

        Err(RustADBError::USBNoDescriptorFound)
    }

    fn write_bulk_data(&self, data: &[u8], timeout: Duration) -> Result<()> {
        let max_packet_size = self.write_info()?.max_packet_size;
        let mut connection = self.write_connection.lock()?;
        let endpoint = connection
            .write_endpoint
            .as_mut()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no write endpoint setup",
            )))?;

        let mut offset = 0;
        let data_len = data.len();
        while offset < data_len {
            let end = (offset + max_packet_size).min(data_len);
            let chunk = Buffer::from(&data[offset..end]);
            let completion = endpoint.transfer_blocking(chunk, timeout);
            map_transfer_status(completion.status)?;
            let write_amount = completion.actual_len;
            offset += write_amount;

            log::trace!("wrote chunk of size {write_amount} - {offset}/{data_len}");
        }

        if offset % max_packet_size == 0 {
            log::trace!("must send final zero-length packet");
            let completion = endpoint.transfer_blocking(Buffer::from(&[][..]), timeout);
            map_transfer_status(completion.status)?;
        }

        Ok(())
    }
}

/// Map a `nusb` transfer status into the crate error type.
///
/// `TransferError::Cancelled` is what `transfer_blocking` returns on timeout;
/// it is translated to the dedicated [`RustADBError::UsbTimeout`] so callers
/// (notably the persistent reader loop) can distinguish a normal timeout from
/// a genuine disconnect via a structured match instead of string matching.
fn map_transfer_status(status: std::result::Result<(), TransferError>) -> Result<()> {
    match status {
        Ok(()) => Ok(()),
        Err(TransferError::Cancelled) => Err(RustADBError::UsbTimeout),
        Err(e) => Err(e.into()),
    }
}

impl ADBTransport for USBTransport {
    fn connect(&mut self) -> crate::Result<()> {
        let device = self.device_info.open().wait()?;

        let (read_endpoint, write_endpoint) = Self::find_endpoints(&device)?;

        // Both bulk endpoints belong to the same ADB interface; claim it once.
        let interface = match device.claim_interface(read_endpoint.iface).wait() {
            Ok(interface) => interface,
            // busy state likely indicates an ADB server is running and has taken the lock over the device
            Err(e) if e.kind() == nusb::ErrorKind::Busy => return Err(RustADBError::DeviceBusy),
            Err(e) => return Err(e.into()),
        };

        let read_ep = interface.endpoint::<Bulk, In>(read_endpoint.address)?;
        log::debug!("got read endpoint: {read_endpoint:?}");

        let write_ep = interface.endpoint::<Bulk, Out>(write_endpoint.address)?;
        log::debug!("got write endpoint: {write_endpoint:?}");

        {
            let mut write_connection = self.write_connection.lock()?;
            write_connection.write_info = Some(write_endpoint);
            write_connection.write_endpoint = Some(write_ep);
        }

        let mut connection = self.connection.lock()?;
        connection.read_info = Some(read_endpoint);
        connection.read_endpoint = Some(read_ep);
        connection.interface = Some(interface);

        Ok(())
    }

    fn disconnect(&mut self) -> crate::Result<()> {
        {
            let connection = self.connection.lock()?;
            if connection.interface.is_none() {
                // device has not been initialized, nothing to do
                return Ok(());
            }
        }

        let message = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[])?;
        if let Err(e) = self.write_message(message) {
            log::error!("error while sending CLSE message: {e}");
        }

        // Dropping the endpoints and the interface releases the claim. This is
        // the `nusb` equivalent of the previous explicit `release_interface`.
        {
            let mut write_connection = self.write_connection.lock()?;
            write_connection.write_endpoint = None;
        }
        let mut connection = self.connection.lock()?;
        connection.read_endpoint = None;
        connection.interface = None;
        log::debug!("succesfully released interface");

        Ok(())
    }
}

impl ADBMessageTransport for USBTransport {
    fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        let message_bytes = message.header().as_bytes();
        self.write_bulk_data(&message_bytes, timeout)?;

        log::trace!("successfully write header: {} bytes", message_bytes.len());

        let payload = message.into_payload();
        if !payload.is_empty() {
            self.write_bulk_data(&payload, timeout)?;
            log::trace!("successfully write payload: {} bytes", payload.len());
        }

        Ok(())
    }

    fn read_message_with_timeout(&mut self, timeout: Duration) -> Result<ADBTransportMessage> {
        let max_packet_size = self.read_info()?.max_packet_size;
        let mut connection = self.connection.lock()?;
        let endpoint = connection
            .read_endpoint
            .as_mut()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no read endpoint setup",
            )))?;

        let mut data = [0u8; 24];
        read_exact(endpoint, &mut data, max_packet_size, timeout)?;

        let header = ADBTransportMessageHeader::try_from(data)?;
        log::trace!("received header {header:?}");

        if header.data_length() != 0 {
            let mut msg_data = vec![0_u8; header.data_length() as usize];
            read_exact(endpoint, &mut msg_data, max_packet_size, timeout)?;

            let message = ADBTransportMessage::from_header_and_payload(header, msg_data);

            // Check message integrity
            if !message.check_message_integrity() {
                return Err(RustADBError::InvalidIntegrity(
                    ADBTransportMessageHeader::compute_crc32(message.payload()),
                    message.header().data_crc32(),
                ));
            }

            return Ok(message);
        }

        Ok(ADBTransportMessage::from_header_and_payload(header, vec![]))
    }
}

/// Read exactly `out.len()` bytes from a bulk IN endpoint into `out`.
///
/// `nusb` requires the requested length of an IN transfer to be a nonzero
/// multiple of `max_packet_size`, and a short packet ends the transfer early.
/// We therefore request `max_packet_size`-aligned buffers and accumulate until
/// `out` is filled, copying out only what was actually received per transfer —
/// preserving the previous fill-loop semantics over `read_bulk`.
fn read_exact(
    endpoint: &mut nusb::Endpoint<Bulk, In>,
    out: &mut [u8],
    max_packet_size: usize,
    timeout: Duration,
) -> Result<()> {
    let mut offset = 0;
    while offset < out.len() {
        let remaining = out.len() - offset;
        // Align the requested length up to a nonzero multiple of the max packet size.
        let request_len = aligned_request_len(remaining, max_packet_size);
        let completion = endpoint.transfer_blocking(Buffer::new(request_len), timeout);
        map_transfer_status(completion.status)?;

        let received = &completion.buffer[..completion.actual_len];
        if received.is_empty() {
            // A zero-length completion with no error would otherwise spin forever.
            return Err(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "USB bulk read returned no data",
            )));
        }

        let copy_len = received.len().min(remaining);
        out[offset..offset + copy_len].copy_from_slice(&received[..copy_len]);
        offset += copy_len;
    }
    Ok(())
}

/// Round `remaining` up to a nonzero multiple of `max_packet_size`.
///
/// `nusb` requires the `requested_len` of an IN transfer to be a nonzero
/// multiple of the endpoint's maximum packet size, otherwise the transfer
/// fails with `TransferError::InvalidArgument`.
fn aligned_request_len(remaining: usize, max_packet_size: usize) -> usize {
    remaining.div_ceil(max_packet_size) * max_packet_size
}

#[cfg(test)]
mod tests {
    use super::{aligned_request_len, map_transfer_status};
    use crate::RustADBError;
    use nusb::transfer::TransferError;

    #[test]
    fn cancelled_status_maps_to_usb_timeout() {
        // The reader loop relies on this: a `transfer_blocking` timeout surfaces
        // as `TransferError::Cancelled` and MUST become `RustADBError::UsbTimeout`
        // so a normal poll timeout is not misclassified as a disconnect.
        let mapped = map_transfer_status(Err(TransferError::Cancelled));
        assert!(
            matches!(mapped, Err(RustADBError::UsbTimeout)),
            "Cancelled must map to UsbTimeout, got {mapped:?}"
        );
    }

    #[test]
    fn ok_status_maps_to_ok() {
        assert!(
            map_transfer_status(Ok(())).is_ok(),
            "successful transfer must map to Ok"
        );
    }

    #[test]
    fn other_transfer_errors_are_not_timeouts() {
        // A genuine disconnect / stall must NOT be classified as a timeout,
        // otherwise the reader loop would keep looping on a dead pipe.
        for err in [
            TransferError::Disconnected,
            TransferError::Stall,
            TransferError::Fault,
            TransferError::InvalidArgument,
            TransferError::Unknown(0),
        ] {
            let mapped = map_transfer_status(Err(err));
            assert!(
                matches!(mapped, Err(RustADBError::UsbTransferError(_))),
                "{err:?} must map to UsbTransferError (not UsbTimeout), got {mapped:?}"
            );
        }
    }

    #[test]
    fn in_transfer_length_is_aligned_to_max_packet_size() {
        // nusb requires the IN requested_len to be a nonzero multiple of the
        // endpoint max packet size; a short packet still ends the transfer early.
        assert_eq!(
            aligned_request_len(24, 512),
            512,
            "24 bytes -> one 512 packet"
        );
        assert_eq!(
            aligned_request_len(512, 512),
            512,
            "exact multiple unchanged"
        );
        assert_eq!(
            aligned_request_len(513, 512),
            1024,
            "513 bytes -> two packets"
        );
        assert_eq!(aligned_request_len(1, 64), 64, "1 byte -> one 64 packet");
        assert_eq!(aligned_request_len(64, 64), 64, "exact multiple unchanged");
        assert_eq!(
            aligned_request_len(65, 64),
            128,
            "65 bytes -> two 64 packets"
        );
    }
}
