use image::{ImageBuffer, Rgba};
use tokio::io::AsyncReadExt;

use crate::{
    Result, RustADBError,
    models::{ADBCommand, ADBLocalCommand, FrameBufferInfoV1, FrameBufferInfoV2},
    proxy::ADBProxyDevice,
};

/// Validate a device-reported framebuffer `size` and return it as a `usize`
/// allocation length.
///
/// `size`, `width`, and `height` are all device-controlled `u32`s read straight
/// off the wire. Allocating `vec![0u8; size]` blindly lets a hostile/garbled
/// header drive a multi-GiB allocation (`size` up to ~4 GiB) before any pixel
/// data is read. Both framebuffer formats here are 4 bytes per pixel
/// (`RGBA_8888` / `RGBX_8888`), so the only valid `size` is `width * height * 4`:
/// compute that with checked arithmetic and reject any frame whose reported
/// `size` does not match. This bounds the allocation to the real image dimensions
/// and doubles as a correctness check (a mismatched `size` would fail
/// `ImageBuffer::from_vec`).
fn checked_framebuffer_len(size: u32, width: u32, height: u32) -> Result<usize> {
    let expected = u64::from(width)
        .checked_mul(u64::from(height))
        .and_then(|px| px.checked_mul(4))
        .ok_or(RustADBError::FramebufferConversionError)?;
    if u64::from(size) != expected {
        return Err(RustADBError::ADBRequestFailed(format!(
            "framebuffer size {size} does not match width*height*4 ({expected}); refusing to allocate"
        )));
    }
    usize::try_from(size).map_err(|_| RustADBError::ConversionError)
}

impl ADBProxyDevice {
    /// Inner method requesting framebuffer from Android device
    pub(crate) async fn framebuffer_inner(&mut self) -> Result<ImageBuffer<Rgba<u8>, Vec<u8>>> {
        self.set_serial_transport().await?;

        self.transport
            .send_adb_request(&ADBCommand::Local(ADBLocalCommand::FrameBuffer))
            .await?;

        let version = self.transport.get_raw_connection()?.read_u32_le().await?;

        match version {
            // RGBA_8888
            1 => {
                let mut buf = [0u8; std::mem::size_of::<FrameBufferInfoV1>()];

                self.transport
                    .get_raw_connection()?
                    .read_exact(&mut buf)
                    .await?;

                let framebuffer_info: FrameBufferInfoV1 = buf.try_into()?;

                let mut data = vec![
                    0_u8;
                    checked_framebuffer_len(
                        framebuffer_info.size,
                        framebuffer_info.width,
                        framebuffer_info.height,
                    )?
                ];
                self.transport
                    .get_raw_connection()?
                    .read_exact(&mut data)
                    .await?;

                Ok(ImageBuffer::<Rgba<u8>, Vec<u8>>::from_vec(
                    framebuffer_info.width,
                    framebuffer_info.height,
                    data,
                )
                .ok_or_else(|| RustADBError::FramebufferConversionError)?)
            }
            // RGBX_8888
            2 => {
                let mut buf = [0u8; std::mem::size_of::<FrameBufferInfoV2>()];

                self.transport
                    .get_raw_connection()?
                    .read_exact(&mut buf)
                    .await?;

                let framebuffer_info: FrameBufferInfoV2 = buf.try_into()?;

                let mut data = vec![
                    0_u8;
                    checked_framebuffer_len(
                        framebuffer_info.size,
                        framebuffer_info.width,
                        framebuffer_info.height,
                    )?
                ];
                self.transport
                    .get_raw_connection()?
                    .read_exact(&mut data)
                    .await?;

                Ok(ImageBuffer::<Rgba<u8>, Vec<u8>>::from_vec(
                    framebuffer_info.width,
                    framebuffer_info.height,
                    data,
                )
                .ok_or_else(|| RustADBError::FramebufferConversionError)?)
            }
            v => Err(RustADBError::UnimplementedFramebufferImageVersion(v)),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::checked_framebuffer_len;
    use crate::RustADBError;

    #[test]
    fn accepts_matching_size() {
        // 1920x1080 RGBA = width*height*4.
        let len = checked_framebuffer_len(1920 * 1080 * 4, 1920, 1080)
            .expect("a size equal to width*height*4 is accepted");
        assert_eq!(len, 1920 * 1080 * 4, "the validated length is the size");
    }

    #[test]
    fn rejects_size_mismatch() {
        // A hostile header claiming a ~4 GiB size with tiny dimensions must be
        // rejected BEFORE allocating, not trusted.
        assert!(
            matches!(
                checked_framebuffer_len(u32::MAX, 16, 16),
                Err(RustADBError::ADBRequestFailed(_))
            ),
            "a size that does not match width*height*4 must be refused"
        );
    }

    #[test]
    fn rejects_zero_size_for_nonzero_dimensions() {
        assert!(
            checked_framebuffer_len(0, 16, 16).is_err(),
            "size 0 with non-zero dimensions is a mismatch"
        );
    }

    #[test]
    fn zero_dimensions_require_zero_size() {
        let len = checked_framebuffer_len(0, 0, 0).expect("0x0 with size 0 is consistent");
        assert_eq!(len, 0, "an empty framebuffer allocates nothing");
    }
}
