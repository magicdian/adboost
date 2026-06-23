use std::future::Future;
use std::path::Path;
use std::pin::Pin;

use tokio::io::{AsyncRead, AsyncWrite};

#[cfg(feature = "framebuffer")]
use {
    image::{ImageBuffer, ImageFormat, Rgba},
    std::io::Cursor,
};

use crate::models::{ADBListItemType, AdbStatResponse, RemountInfo};
use crate::{ADBStatExtendedResponse, RebootType, Result};

/// Trait representing all features available on ADB devices.
///
/// All methods are `async`. [`trait_variant::make`] generates a `Send` variant
/// of every returned future so devices can be driven from a multi-threaded
/// tokio runtime. The `shell`/`exec`/`pull`/`push`/`shell_command` byte-stream
/// parameters are `tokio::io::{AsyncRead, AsyncWrite}` trait objects (the
/// breaking-change decision from the async rewrite PRD); consumers bridge sync
/// `File`/`Vec` via `tokio_util::compat` or `tokio::fs`.
///
/// Object safety note: with AFIT + the `trait_variant` `Send` variant the trait
/// is **not** `dyn`-compatible (async-fn-in-trait desugars to RPITIT, which is
/// not object-safe in stable Rust). The previous `boxed()` helper that returned
/// `Box<dyn ADBDeviceExt>` is therefore removed; `dyn`-erasure for consumers is
/// deferred to the consumer-adaptation task (e.g. via `dynosaur` or a concrete
/// enum), per the PRD escape hatch.
#[trait_variant::make(Send)]
pub trait ADBDeviceExt {
    /// Runs command in a shell on the device, and write its output and error streams into output.
    async fn shell_command(
        &mut self,
        command: &(dyn AsRef<str> + Sync),
        stdout: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
        stderr: Option<&mut (dyn AsyncWrite + Unpin + Send)>,
    ) -> Result<Option<u8>>;

    /// Starts an interactive shell session on the device.
    /// Input data is read from reader and write to writer.
    async fn shell(
        &mut self,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()>;

    /// Runs command on the device.
    /// Input data is read from reader and write to writer.
    async fn exec(
        &mut self,
        command: &str,
        reader: &mut (dyn AsyncRead + Unpin + Send),
        writer: Pin<Box<dyn AsyncWrite + Send>>,
    ) -> Result<()>;

    /// Display the stat information for a remote file using STAT protocol command.
    async fn stat(&mut self, remote_path: &(dyn AsRef<str> + Sync)) -> Result<AdbStatResponse>;

    /// Display the stat information for a remote file using `stat` shell command.
    /// This is an extended version of `stat` that returns more detailed information.
    /// Returns `Ok(None)` if the file does not exist on the device.
    fn stat_extended(
        &mut self,
        remote_path: &(dyn AsRef<str> + Sync),
    ) -> impl Future<Output = Result<Option<ADBStatExtendedResponse>>> + Send {
        async move {
            let mut stdout = Vec::new();
            self.shell_command(
                &format!("stat {}", remote_path.as_ref()),
                Some(&mut stdout),
                None,
            )
            .await?;

            // all parsing magic happens here...
            ADBStatExtendedResponse::try_from(&stdout)
        }
    }

    /// Pull the remote file pointed to by `source` and write its contents into `output`
    async fn pull(
        &mut self,
        source: &(dyn AsRef<str> + Sync),
        output: &mut (dyn AsyncWrite + Unpin + Send),
    ) -> Result<()>;

    /// Push `stream` to `path` on the device.
    async fn push(
        &mut self,
        stream: &mut (dyn AsyncRead + Unpin + Send),
        path: &(dyn AsRef<str> + Sync),
    ) -> Result<()>;

    /// List the items in a directory on the device
    async fn list(&mut self, path: &(dyn AsRef<str> + Sync)) -> Result<Vec<ADBListItemType>>;

    /// Reboot the device using given reboot type
    async fn reboot(&mut self, reboot_type: RebootType) -> Result<()>;

    /// Remount the device partitions as read-write
    async fn remount(&mut self) -> Result<Vec<RemountInfo>>;

    /// Restart adb daemon with root permissions
    async fn root(&mut self) -> Result<()>;

    /// Restart adb daemon without root permissions
    async fn unroot(&mut self) -> Result<()>;

    /// Run `activity` from `package` on device. Return the command output.
    fn run_activity(
        &mut self,
        package: &(dyn AsRef<str> + Sync),
        activity: &(dyn AsRef<str> + Sync),
    ) -> impl Future<Output = Result<Vec<u8>>> + Send {
        async move {
            let mut output = Vec::new();
            let _status = self
                .shell_command(
                    &format!(
                        "am start {}/{}.{}",
                        package.as_ref(),
                        package.as_ref(),
                        activity.as_ref()
                    ),
                    Some(&mut output),
                    None,
                )
                .await?;

            Ok(output)
        }
    }

    /// Install an APK pointed to by `apk_path` on device.
    async fn install(
        &mut self,
        apk_path: &(dyn AsRef<Path> + Sync),
        user: Option<&str>,
    ) -> Result<()>;

    /// Uninstall the package `package` from device.
    async fn uninstall(
        &mut self,
        package: &(dyn AsRef<str> + Sync),
        user: Option<&str>,
    ) -> Result<()>;

    /// Enable dm-verity on the device
    async fn enable_verity(&mut self) -> Result<()>;

    /// Disable dm-verity on the device
    async fn disable_verity(&mut self) -> Result<()>;

    /// Restart the device's adbd in TCP/IP mode, listening on `port` (the
    /// `adb tcpip <port>` operation).
    ///
    /// Returns the device's textual acknowledgement (e.g.
    /// `restarting in TCP mode port: 5555`). After this succeeds adbd restarts,
    /// so a direct (USB) connection that issued it is expected to drop; reconnect
    /// over TCP with [`crate::usb::ADBUSBDevice`]'s TCP counterpart or
    /// `adb connect <ip>:<port>`.
    async fn tcpip(&mut self, port: u16) -> Result<String>;

    /// Restart the device's adbd in USB mode (the `adb usb` operation), undoing a
    /// previous [`Self::tcpip`]. Like `tcpip`, adbd restarts on success.
    async fn usb(&mut self) -> Result<()>;

    #[cfg(feature = "framebuffer")]
    /// Inner method requesting framebuffer from an Android device
    async fn framebuffer_inner(&mut self) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>>;

    /// Dump framebuffer of this device into given path.
    ///
    /// Output data format is currently only `PNG`.
    #[cfg(feature = "framebuffer")]
    fn framebuffer<'a>(
        &'a mut self,
        path: &'a (dyn AsRef<Path> + Sync),
    ) -> impl Future<Output = Result<()>> + Send + 'a {
        async move {
            // Big help from AOSP source code (<https://android.googlesource.com/platform/system/adb/+/refs/heads/main/framebuffer_service.cpp>)
            let img = self.framebuffer_inner().await?;
            Ok(img.save(path.as_ref())?)
        }
    }

    /// Dump framebuffer of this device and return corresponding bytes.
    ///
    /// Output data format is currently only `PNG`.
    #[cfg(feature = "framebuffer")]
    fn framebuffer_bytes(&mut self) -> impl Future<Output = Result<Vec<u8>>> + Send {
        async move {
            let img = self.framebuffer_inner().await?;
            let mut vec = Cursor::new(Vec::new());
            img.write_to(&mut vec, ImageFormat::Png)?;

            Ok(vec.into_inner())
        }
    }
}
