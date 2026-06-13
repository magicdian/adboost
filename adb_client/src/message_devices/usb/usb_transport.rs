use std::{sync::Arc, time::Duration};

use nusb::{
    DeviceInfo, Interface,
    descriptors::TransferType,
    transfer::{Buffer, Bulk, Completion, Direction, EndpointDirection, In, Out, TransferError},
};
use tokio::sync::Mutex;

use crate::{
    Result, RustADBError,
    adb_transport::ADBTransport,
    message_devices::{
        adb_message_transport::ADBMessageTransport,
        adb_transport_message::{
            ADBTransportMessage, ADBTransportMessageHeader, MAX_PAYLOAD, payload_len_within_bound,
        },
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
/// endpoints live behind shared async `Mutex`es. Cloning a `USBTransport`
/// shares the *same* underlying handle — this mirrors the previous `rusb`
/// behavior where clones shared an `Arc<DeviceHandle>`, which the
/// reader-task / writer concurrency model in `ADBMessageDevice` and the
/// persistent connection rely upon.
///
/// The IN (read) and OUT (write) endpoints use **separate** locks so a reader
/// task awaiting a long IN transfer never blocks a concurrent writer on the OUT
/// endpoint. This preserves the independent reader/writer concurrency the
/// previous implementation had with two distinct endpoints sharing one handle.
///
/// These are [`tokio::sync::Mutex`]es (not `std::sync::Mutex`): a transfer holds
/// the endpoint exclusively across its `submit` / `next_complete().await`, so
/// the guard must be `Send` and live across `.await`. The async mutex is the
/// exclusivity mechanism for the endpoint queue — only one transfer is ever
/// pending per endpoint, which the queue model (`pending() == 0` between calls)
/// requires.
#[derive(Debug, Default)]
struct Connection {
    interface: Option<Interface>,
    read_endpoint: Option<nusb::Endpoint<Bulk, In>>,
    read_info: Option<EndpointInfo>,
    /// Bytes received by a bulk IN completion that overshot the logically
    /// requested length (the next frame's header — or the start of a payload —
    /// that the device/host controller coalesced into the same transfer as the
    /// current frame). They are carried here and consumed by the *next*
    /// [`read_exact`] before another transfer is submitted, so the framed stream
    /// stays aligned. See [`read_exact`] for why this happens on the reverse
    /// (device→host bulk) path.
    read_residual: Vec<u8>,
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
    pub async fn new(vendor_id: u16, product_id: u16) -> Result<Self> {
        for device_info in nusb::list_devices().await? {
            if device_info.vendor_id() == vendor_id && device_info.product_id() == product_id {
                return Ok(Self::new_from_device(device_info));
            }
        }

        Err(RustADBError::DeviceNotFound(format!(
            "cannot find USB device with vendor_id={vendor_id} and product_id={product_id}",
        )))
    }

    /// Instantiate a new [`USBTransport`] selected by its USB serial number.
    ///
    /// Unlike [`Self::new`] (which matches on `vendor_id`/`product_id` and
    /// returns the *first* match), this matches the device's iSerial descriptor
    /// — the identifier `adb devices` shows — so it unambiguously selects one
    /// device even when several share the same `vendor_id`/`product_id`.
    pub async fn new_by_serial(serial: &str) -> Result<Self> {
        for device_info in nusb::list_devices().await? {
            if device_info.serial_number() == Some(serial) {
                return Ok(Self::new_from_device(device_info));
            }
        }

        Err(RustADBError::DeviceNotFound(format!(
            "cannot find USB device with serial={serial}",
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

    async fn write_bulk_data(&self, data: &[u8], timeout: Duration) -> Result<()> {
        let mut connection = self.write_connection.lock().await;
        let max_packet_size = connection
            .write_info
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no write endpoint setup",
            )))?
            .max_packet_size;
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
            let completion = transfer_with_timeout(endpoint, chunk, timeout).await;
            map_transfer_status(completion.status)?;
            let write_amount = completion.actual_len;
            offset += write_amount;

            tracing::trace!("wrote chunk of size {write_amount} - {offset}/{data_len}");
        }

        if offset % max_packet_size == 0 {
            tracing::trace!("must send final zero-length packet");
            let completion = transfer_with_timeout(endpoint, Buffer::from(&[][..]), timeout).await;
            map_transfer_status(completion.status)?;
        }

        Ok(())
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
}

/// Submit a single transfer on a bulk endpoint and await its completion under a
/// timeout, using `nusb`'s queue model.
///
/// `nusb` 0.2 endpoints are a transfer *queue*: `submit(buf)` enqueues a
/// transfer and `next_complete().await` (cancel-safe) yields the next
/// completion. There is no built-in per-transfer timeout, so we wrap
/// `next_complete()` in [`tokio::time::timeout`].
///
/// On timeout we replicate the synchronous `transfer_blocking` cleanup exactly:
/// request cancellation of the still-pending transfer (`cancel_all`) and then
/// drain it — a cancelled transfer is still returned from `next_complete`, so
/// the endpoint queue returns to `pending() == 0` and is never left with a
/// dangling transfer. We then force the status to `TransferError::Cancelled`,
/// which [`map_transfer_status`] turns into [`RustADBError::UsbTimeout`],
/// preserving the timeout-vs-disconnect distinction the persistent reader loop
/// relies on (even in the rare race where the transfer completed during
/// cancellation, a timeout must never surface as a successful transfer).
async fn transfer_with_timeout<Dir>(
    endpoint: &mut nusb::Endpoint<Bulk, Dir>,
    buf: Buffer,
    timeout: Duration,
) -> Completion
where
    Dir: EndpointDirection,
{
    endpoint.submit(buf);

    match tokio::time::timeout(timeout, endpoint.next_complete()).await {
        Ok(completion) => completion,
        Err(_elapsed) => {
            endpoint.cancel_all();
            let completion = endpoint.next_complete().await;
            Completion {
                status: Err(TransferError::Cancelled),
                ..completion
            }
        }
    }
}

/// Map a `nusb` transfer status into the crate error type.
///
/// `TransferError::Cancelled` is what a timed-out transfer surfaces (see
/// [`transfer_with_timeout`]); it is translated to the dedicated
/// [`RustADBError::UsbTimeout`] so callers (notably the persistent reader loop)
/// can distinguish a normal timeout from a genuine disconnect via a structured
/// match instead of string matching.
fn map_transfer_status(status: std::result::Result<(), TransferError>) -> Result<()> {
    match status {
        Ok(()) => Ok(()),
        Err(TransferError::Cancelled) => Err(RustADBError::UsbTimeout),
        Err(e) => Err(e.into()),
    }
}

impl ADBTransport for USBTransport {
    async fn connect(&mut self) -> crate::Result<()> {
        let device = self.device_info.open().await?;

        let (read_endpoint, write_endpoint) = Self::find_endpoints(&device)?;

        // Both bulk endpoints belong to the same ADB interface; claim it once.
        let interface = match device.claim_interface(read_endpoint.iface).await {
            Ok(interface) => interface,
            // busy state likely indicates an ADB server is running and has taken the lock over the device
            Err(e) if e.kind() == nusb::ErrorKind::Busy => return Err(RustADBError::DeviceBusy),
            Err(e) => return Err(e.into()),
        };

        let read_ep = interface.endpoint::<Bulk, In>(read_endpoint.address)?;
        tracing::debug!("got read endpoint: {read_endpoint:?}");

        let write_ep = interface.endpoint::<Bulk, Out>(write_endpoint.address)?;
        tracing::debug!("got write endpoint: {write_endpoint:?}");

        {
            let mut write_connection = self.write_connection.lock().await;
            write_connection.write_info = Some(write_endpoint);
            write_connection.write_endpoint = Some(write_ep);
        }

        let mut connection = self.connection.lock().await;
        connection.read_info = Some(read_endpoint);
        connection.read_endpoint = Some(read_ep);
        connection.interface = Some(interface);
        // Drop any bytes carried over from a previous connection's framed stream:
        // a fresh CNXN handshake must never consume a stale CLSE/WRTE left in the
        // residual buffer (that desyncs the handshake — "expected CNXN, got CLSE").
        connection.read_residual.clear();

        Ok(())
    }

    async fn disconnect(&mut self) -> crate::Result<()> {
        {
            let connection = self.connection.lock().await;
            if connection.interface.is_none() {
                // device has not been initialized, nothing to do
                return Ok(());
            }
        }

        let message = ADBTransportMessage::try_new(MessageCommand::Clse, 0, 0, &[])?;
        if let Err(e) = self.write_message(message).await {
            tracing::error!("error while sending CLSE message: {e}");
        }

        // Dropping the endpoints and the interface releases the claim. This is
        // the `nusb` equivalent of the previous explicit `release_interface`.
        {
            let mut write_connection = self.write_connection.lock().await;
            write_connection.write_endpoint = None;
        }
        let mut connection = self.connection.lock().await;
        connection.read_endpoint = None;
        connection.interface = None;
        connection.read_residual.clear();
        tracing::debug!("succesfully released interface");

        Ok(())
    }
}

impl ADBMessageTransport for USBTransport {
    async fn write_message_with_timeout(
        &mut self,
        message: ADBTransportMessage,
        timeout: Duration,
    ) -> Result<()> {
        let message_bytes = message.header().as_bytes();
        self.write_bulk_data(&message_bytes, timeout).await?;

        tracing::trace!("successfully write header: {} bytes", message_bytes.len());

        let payload = message.into_payload();
        if !payload.is_empty() {
            self.write_bulk_data(&payload, timeout).await?;
            tracing::trace!("successfully write payload: {} bytes", payload.len());
        }

        Ok(())
    }

    async fn read_message_with_timeout(
        &mut self,
        timeout: Duration,
    ) -> Result<ADBTransportMessage> {
        let mut connection = self.connection.lock().await;
        let max_packet_size = connection
            .read_info
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no read endpoint setup",
            )))?
            .max_packet_size;
        // Split the borrow: the IN transfer needs `&mut read_endpoint`, while the
        // overflow carry-over needs `&mut read_residual`. Reborrowing the struct
        // fields individually keeps both alive across the await inside `read_exact`.
        let Connection {
            read_endpoint,
            read_residual,
            ..
        } = &mut *connection;
        let endpoint = read_endpoint
            .as_mut()
            .ok_or(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::NotConnected,
                "no read endpoint setup",
            )))?;

        let mut data = [0u8; 24];
        read_exact(endpoint, read_residual, &mut data, max_packet_size, timeout).await?;

        let header = ADBTransportMessageHeader::try_from(data)?;
        tracing::trace!("received header {header:?}");

        // Bound the wire data_length BEFORE allocating (AOSP check_header clause:
        // reject data_length > MAX_PAYLOAD before reading the payload). A hostile or
        // corrupt 24-byte header could otherwise drive a ~4 GiB allocation.
        if !payload_len_within_bound(header.data_length()) {
            return Err(RustADBError::ADBRequestFailed(format!(
                "frame data_length {} exceeds MAX_PAYLOAD {MAX_PAYLOAD}",
                header.data_length()
            )));
        }

        let payload = if header.data_length() != 0 {
            let mut msg_data = vec![0_u8; header.data_length() as usize];
            read_exact(
                endpoint,
                read_residual,
                &mut msg_data,
                max_packet_size,
                timeout,
            )
            .await?;
            msg_data
        } else {
            vec![]
        };

        let message = ADBTransportMessage::from_header_and_payload(header, payload);

        // Check message integrity (magic-only; runs for every frame, AOSP-faithful)
        if !message.check_message_integrity() {
            return Err(RustADBError::InvalidIntegrity(
                ADBTransportMessageHeader::compute_magic(message.header().command()),
                message.header().magic(),
            ));
        }

        Ok(message)
    }
}

/// Read exactly `out.len()` bytes from a bulk IN endpoint into `out`, carrying
/// any over-read bytes across calls via `residual`.
///
/// `nusb` requires the requested length of an IN transfer to be a nonzero
/// multiple of `max_packet_size`, and a short packet ends the transfer early.
/// We therefore request `max_packet_size`-aligned buffers and accumulate until
/// `out` is filled.
///
/// # Why over-read happens (and must be carried, not discarded)
///
/// A bulk IN completion can return up to `request_len` (the aligned length)
/// bytes, which may be MORE than the `out.len()` we logically need for the
/// current frame field. Under sustained device→host throughput (the `reverse`
/// data plane) the device/host controller coalesces a frame's payload tail and
/// the *next* frame's 24-byte header (or the start of the next payload) into one
/// transfer. Earlier code treated this overshoot as a fatal "frame desync"
/// error, which tore the whole multiplexed connection down on the first bulk
/// reverse frame (every session's channel closed → 0 bytes transferred).
///
/// The correct behavior — matching a normal buffered stream reader — is to copy
/// out exactly what the current field needs and stash the remainder in
/// `residual`, consuming it first on the next call. This keeps the framed stream
/// aligned without a fatal error. (The forward/opener path never tripped this
/// because device→host reads there are small/aligned control frames.)
async fn read_exact(
    endpoint: &mut nusb::Endpoint<Bulk, In>,
    residual: &mut Vec<u8>,
    out: &mut [u8],
    max_packet_size: usize,
    timeout: Duration,
) -> Result<()> {
    let mut offset = 0;

    // Drain any bytes left over from a previous over-read first.
    if !residual.is_empty() {
        let take = residual.len().min(out.len());
        out[..take].copy_from_slice(&residual[..take]);
        residual.drain(..take);
        offset += take;
    }

    while offset < out.len() {
        let remaining = out.len() - offset;
        // Align the requested length up to a nonzero multiple of the max packet size.
        let request_len = aligned_request_len(remaining, max_packet_size);
        let completion = transfer_with_timeout(endpoint, Buffer::new(request_len), timeout).await;
        map_transfer_status(completion.status)?;

        let received = &completion.buffer[..completion.actual_len];
        if received.is_empty() {
            // A zero-length completion with no error would otherwise spin forever.
            return Err(RustADBError::IOError(std::io::Error::new(
                std::io::ErrorKind::UnexpectedEof,
                "USB bulk read returned no data",
            )));
        }

        // Copy what this frame field needs; stash any overshoot for the next call.
        let copy_len = fill_and_carry(received, &mut out[offset..], residual);
        offset += copy_len;
    }
    Ok(())
}

/// Copy as much of `received` as fits into `dst`, returning the number of bytes
/// written. Any bytes beyond `dst.len()` are appended to `residual` (the
/// over-read carry-over for the next [`read_exact`] field). Pure / I/O-free so
/// the over-read carry logic is unit-testable.
fn fill_and_carry(received: &[u8], dst: &mut [u8], residual: &mut Vec<u8>) -> usize {
    let copy_len = received.len().min(dst.len());
    dst[..copy_len].copy_from_slice(&received[..copy_len]);
    if received.len() > copy_len {
        residual.extend_from_slice(&received[copy_len..]);
    }
    copy_len
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
    use super::{aligned_request_len, fill_and_carry, map_transfer_status};
    use crate::RustADBError;
    use nusb::transfer::TransferError;

    #[test]
    fn cancelled_status_maps_to_usb_timeout() {
        // The reader loop relies on this: a timed-out transfer surfaces as
        // `TransferError::Cancelled` and MUST become `RustADBError::UsbTimeout`
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

    #[test]
    fn fill_and_carry_exact_fit_leaves_no_residual() {
        // A completion that exactly fills the requested field: copy all, no carry.
        let mut dst = [0u8; 4];
        let mut residual = Vec::new();
        let n = fill_and_carry(&[1, 2, 3, 4], &mut dst, &mut residual);
        assert_eq!(n, 4, "all 4 bytes copied");
        assert_eq!(dst, [1, 2, 3, 4]);
        assert!(residual.is_empty(), "exact fit → no residual");
    }

    #[test]
    fn fill_and_carry_overshoot_stashes_remainder() {
        // The reverse-data-plane case: the device coalesced the next frame's
        // header into this transfer. Copy only what `dst` needs; carry the rest.
        let mut dst = [0u8; 2];
        let mut residual = Vec::new();
        let n = fill_and_carry(&[10, 20, 30, 40, 50], &mut dst, &mut residual);
        assert_eq!(n, 2, "only 2 bytes fit dst");
        assert_eq!(dst, [10, 20]);
        assert_eq!(residual, vec![30, 40, 50], "overshoot carried to residual");
    }

    #[test]
    fn fill_and_carry_short_packet_partial_fill() {
        // A short packet (fewer bytes than the field needs): copy what arrived,
        // no carry; the caller loops for the rest.
        let mut dst = [0u8; 8];
        let mut residual = Vec::new();
        let n = fill_and_carry(&[1, 2, 3], &mut dst, &mut residual);
        assert_eq!(n, 3, "only 3 bytes arrived");
        assert_eq!(&dst[..3], &[1, 2, 3]);
        assert!(residual.is_empty(), "short packet → nothing to carry");
    }

    #[test]
    fn fill_and_carry_appends_to_existing_residual() {
        // Residual accumulates across calls without clobbering prior carry-over.
        let mut dst = [0u8; 1];
        let mut residual = vec![99];
        let n = fill_and_carry(&[7, 8, 9], &mut dst, &mut residual);
        assert_eq!(n, 1);
        assert_eq!(dst, [7]);
        assert_eq!(
            residual,
            vec![99, 8, 9],
            "new overshoot appended after existing"
        );
    }
}
