use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

use crate::{
    ADBDeviceExt, ADBListItemType, RebootType, Result,
    message_devices::{
        adb_message_device::ADBMessageDevice, adb_message_transport::ADBMessageTransport,
    },
    models::{AdbStatResponse, RemountInfo},
};
use std::path::Path;

impl<T: ADBMessageTransport> ADBDeviceExt for ADBMessageDevice<T> {
    async fn shell_command(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>> {
        self.shell_command(command, stdout, stderr).await
    }

    async fn shell(
        &mut self,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.shell(reader, writer).await
    }

    async fn exec(
        &mut self,
        command: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()> {
        self.exec(command, reader, writer).await
    }

    async fn stat(&mut self, remote_path: &(dyn AsRef<str> + Sync)) -> Result<AdbStatResponse> {
        self.stat(remote_path).await
    }

    async fn pull(
        &mut self,
        source: &(dyn AsRef<str> + Sync),
        output: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<()> {
        self.pull(source.as_ref(), output).await
    }

    async fn push(
        &mut self,
        stream: &mut (dyn AsyncRead + Unpin + Send),
        path: &(dyn AsRef<str> + Sync),
    ) -> Result<()> {
        self.push(stream, path.as_ref()).await
    }

    async fn reboot(&mut self, reboot_type: RebootType) -> Result<()> {
        self.reboot(reboot_type).await
    }

    async fn remount(&mut self) -> Result<Vec<RemountInfo>> {
        self.remount().await
    }

    async fn root(&mut self) -> Result<()> {
        self.root().await
    }

    async fn install(
        &mut self,
        apk_path: &(dyn AsRef<Path> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.install(apk_path, user).await
    }

    async fn uninstall(
        &mut self,
        package: &(dyn AsRef<str> + Sync),
        user: Option<&str>,
    ) -> Result<()> {
        self.uninstall(package, user).await
    }

    async fn enable_verity(&mut self) -> Result<()> {
        self.enable_verity().await
    }

    async fn disable_verity(&mut self) -> Result<()> {
        self.disable_verity().await
    }

    #[cfg(feature = "framebuffer")]
    async fn framebuffer_inner(&mut self) -> Result<image::ImageBuffer<image::Rgba<u8>, Vec<u8>>> {
        self.framebuffer_inner().await
    }

    async fn list(&mut self, path: &(dyn AsRef<str> + Sync)) -> Result<Vec<ADBListItemType>> {
        self.list(path.as_ref()).await
    }
}
