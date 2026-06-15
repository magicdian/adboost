use std::net::SocketAddr;
use std::path::Path;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::message_devices::adb_message_device::ADBMessageDevice;
use crate::models::RemountInfo;
use crate::tcp::tcp_transport::TcpTransport;
use crate::utils::get_default_adb_key_path;
use crate::{ADBDeviceExt, ADBListItemType, Result};

/// Represent a device reached and available over TCP.
#[derive(Debug)]
pub struct ADBTcpDevice {
    inner: ADBMessageDevice<TcpTransport>,
}

impl ADBTcpDevice {
    /// Instantiate a new [`ADBTcpDevice`]
    pub async fn new<A: Into<SocketAddr>>(address: A) -> Result<Self> {
        Self::new_with_custom_private_key(address, get_default_adb_key_path()?).await
    }

    /// Instantiate a new [`ADBTcpDevice`] using a custom private key path
    pub async fn new_with_custom_private_key<P: AsRef<Path>, A: Into<SocketAddr>>(
        address: A,
        private_key_path: P,
    ) -> Result<Self> {
        Ok(Self {
            inner: ADBMessageDevice::new(
                TcpTransport::new(address, &private_key_path),
                private_key_path,
            )
            .await?,
        })
    }
}

impl ADBDeviceExt for ADBTcpDevice {
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

    async fn tcpip(&mut self, port: u16) -> Result<String> {
        self.inner.tcpip(port).await
    }

    async fn usb(&mut self) -> Result<()> {
        self.inner.usb().await
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
