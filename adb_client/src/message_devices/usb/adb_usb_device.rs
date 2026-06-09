use std::path::Path;
use std::path::PathBuf;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::ADBDeviceExt;
use crate::ADBListItemType;
use crate::Result;
use crate::RustADBError;
use crate::message_devices::adb_message_device::ADBMessageDevice;
use crate::models::RemountInfo;
use crate::usb::usb_transport::USBTransport;
use crate::usb::utils;
use crate::utils::get_default_adb_key_path;

/// Represent a device reached and available over USB.
#[derive(Debug)]
pub struct ADBUSBDevice {
    inner: ADBMessageDevice<USBTransport>,
    vendor_id: u16,
    product_id: u16,
}

impl ADBUSBDevice {
    /// Instantiate a new [`ADBUSBDevice`]
    pub async fn new(vendor_id: u16, product_id: u16) -> Result<Self> {
        Self::new_with_custom_private_key(vendor_id, product_id, get_default_adb_key_path()?).await
    }

    /// Instantiate a new [`ADBUSBDevice`] using a custom private key path
    pub async fn new_with_custom_private_key<P: AsRef<Path>>(
        vendor_id: u16,
        product_id: u16,
        private_key_path: P,
    ) -> Result<Self> {
        Self::new_from_transport_inner(
            USBTransport::new(vendor_id, product_id).await?,
            private_key_path,
        )
        .await
    }

    /// Instantiate a new [`ADBUSBDevice`] from a [`USBTransport`] and an optional private key path.
    pub async fn new_from_transport(
        transport: USBTransport,
        private_key_path: Option<PathBuf>,
    ) -> Result<Self> {
        let private_key_path = match private_key_path {
            Some(private_key_path) => private_key_path,
            None => get_default_adb_key_path()?,
        };

        Self::new_from_transport_inner(transport, &private_key_path).await
    }

    async fn new_from_transport_inner<P: AsRef<Path>>(
        transport: USBTransport,
        private_key_path: P,
    ) -> Result<Self> {
        let vendor_id = transport.vendor_id();
        let product_id = transport.product_id();

        Ok(Self {
            inner: ADBMessageDevice::new(transport, private_key_path).await?,
            vendor_id,
            product_id,
        })
    }

    /// Returns the vendor ID of the device
    #[must_use]
    pub const fn vendor_id(&self) -> u16 {
        self.vendor_id
    }

    /// Returns the product ID of the device
    #[must_use]
    pub const fn product_id(&self) -> u16 {
        self.product_id
    }

    /// Returns a mutable reference to the inner message device for advanced operations.
    pub fn inner_mut(&mut self) -> &mut ADBMessageDevice<USBTransport> {
        &mut self.inner
    }

    /// Autodetect connected ADB devices and establish a connection with the first device found
    ///
    /// # Errors
    ///
    /// Returns an error if multiple devices or none are connected.
    pub async fn autodetect() -> Result<Self> {
        Self::autodetect_with_custom_private_key(get_default_adb_key_path()?).await
    }

    /// Autodetect connected ADB devices and establish a connection with the first device found using a custom private key path
    ///
    /// # Errors
    ///
    /// Returns an error if multiple devices are connected or if none can be detected.
    pub async fn autodetect_with_custom_private_key(private_key_path: PathBuf) -> Result<Self> {
        match utils::get_single_connected_adb_device()? {
            Some(device_info) => {
                Self::new_with_custom_private_key(
                    device_info.vendor_id,
                    device_info.product_id,
                    private_key_path,
                )
                .await
            }
            _ => Err(RustADBError::DeviceNotFound(
                "cannot find USB devices matching the signature of an ADB device".into(),
            )),
        }
    }
}

impl ADBDeviceExt for ADBUSBDevice {
    async fn shell_command(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>> {
        self.inner.shell_command(command, stdout, stderr).await
    }

    async fn shell(
        &mut self,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.inner.shell(reader, writer).await
    }

    async fn stat(
        &mut self,
        remote_path: &(dyn AsRef<str> + Sync),
    ) -> Result<crate::AdbStatResponse> {
        self.inner.stat(remote_path).await
    }

    async fn pull(
        &mut self,
        source: &(dyn AsRef<str> + Sync),
        output: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        self.inner.pull(source, output).await
    }

    async fn push(
        &mut self,
        stream: &mut (dyn AsyncRead + Unpin + Send),
        path: &(dyn AsRef<str> + Sync),
    ) -> Result<()> {
        self.inner.push(stream, path).await
    }

    async fn reboot(&mut self, reboot_type: crate::RebootType) -> Result<()> {
        self.inner.reboot(reboot_type).await
    }

    async fn remount(&mut self) -> Result<Vec<RemountInfo>> {
        self.inner.remount().await
    }

    async fn root(&mut self) -> Result<()> {
        self.inner.root().await
    }

    async fn install(
        &mut self,
        apk_path: &(dyn AsRef<Path> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.inner.install(apk_path, user).await
    }

    async fn uninstall(
        &mut self,
        package: &(dyn AsRef<str> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.inner.uninstall(package, user).await
    }

    async fn enable_verity(&mut self) -> Result<()> {
        self.inner.enable_verity().await
    }

    async fn disable_verity(&mut self) -> Result<()> {
        self.inner.disable_verity().await
    }

    #[cfg(feature = "framebuffer")]
    async fn framebuffer_inner(&mut self) -> Result<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
        self.inner.framebuffer_inner().await
    }

    async fn list(&mut self, path: &(dyn AsRef<str> + Sync)) -> Result<Vec<ADBListItemType>> {
        self.inner.list(path).await
    }

    async fn exec(
        &mut self,
        command: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.inner.exec(command, reader, writer).await
    }
}
