//! Read an image off the Windows clipboard.
//!
//! Only the `CF_DIB` path is implemented: uncompressed 24- and 32-bit bitmaps,
//! which is what screenshots and "copy image" produce. The bytes are converted
//! to RGBA and encoded as PNG by [`crate::png`], because the providers accept
//! PNG/JPEG, not raw DIB.

/// A pasted image, already encoded as PNG.
pub struct Image {
    pub width: u32,
    pub height: u32,
    pub png: Vec<u8>,
}

#[cfg(windows)]
pub fn read_image() -> Result<Image, String> {
    use windows_sys::Win32::System::DataExchange::{
        CloseClipboard, GetClipboardData, IsClipboardFormatAvailable, OpenClipboard,
    };
    /// `CF_DIB` from WINUSER.H; windows-sys does not export it in this module.
    const CF_DIB: u32 = 8;
    use windows_sys::Win32::System::Memory::{GlobalLock, GlobalSize, GlobalUnlock};

    // SAFETY: every call is checked, the clipboard is closed on every path after
    // a successful open, and the global memory is unlocked before returning.
    unsafe {
        if IsClipboardFormatAvailable(CF_DIB) == 0 {
            return Err("the clipboard does not hold an image".to_string());
        }
        if OpenClipboard(std::ptr::null_mut()) == 0 {
            return Err("cannot open the clipboard (another app may hold it)".to_string());
        }
        let handle = GetClipboardData(CF_DIB);
        if handle.is_null() {
            CloseClipboard();
            return Err("cannot read the clipboard image".to_string());
        }
        let size = GlobalSize(handle);
        let pointer = GlobalLock(handle);
        if pointer.is_null() || size == 0 {
            CloseClipboard();
            return Err("cannot lock the clipboard image".to_string());
        }
        let bytes = std::slice::from_raw_parts(pointer as *const u8, size).to_vec();
        GlobalUnlock(handle);
        CloseClipboard();

        let (width, height, rgba) = decode_dib(&bytes)?;
        Ok(Image {
            width,
            height,
            png: crate::png::encode_rgba(width, height, &rgba),
        })
    }
}

#[cfg(not(windows))]
pub fn read_image() -> Result<Image, String> {
    Err("pasting images is only implemented on Windows".to_string())
}

/// Decode a `CF_DIB` payload into RGBA pixels.
///
/// Handles `BI_RGB` with 24 or 32 bits per pixel, either orientation. Anything
/// else is rejected rather than guessed at.
pub fn decode_dib(bytes: &[u8]) -> Result<(u32, u32, Vec<u8>), String> {
    if bytes.len() < 40 {
        return Err("the clipboard bitmap header is truncated".to_string());
    }
    let header_size = u32::from_le_bytes(bytes[0..4].try_into().unwrap()) as usize;
    let width = i32::from_le_bytes(bytes[4..8].try_into().unwrap());
    let height = i32::from_le_bytes(bytes[8..12].try_into().unwrap());
    // biPlanes sits at 12, biBitCount at 14.
    let bits = u16::from_le_bytes(bytes[14..16].try_into().unwrap());
    let compression = u32::from_le_bytes(bytes[16..20].try_into().unwrap());
    let colors = u32::from_le_bytes(bytes[32..36].try_into().unwrap()) as usize;

    if width <= 0 || height == 0 {
        return Err("the clipboard bitmap has no pixels".to_string());
    }
    if compression != 0 {
        return Err("only uncompressed clipboard bitmaps are supported".to_string());
    }
    if bits != 24 && bits != 32 {
        return Err(format!("{bits}-bit clipboard bitmaps are not supported"));
    }

    let width = width as u32;
    let top_down = height < 0;
    let height = height.unsigned_abs();
    let stride = (width * u32::from(bits)).div_ceil(32) * 4;
    let offset = header_size + colors * 4;
    let needed = offset as u64 + u64::from(stride) * u64::from(height);
    if (bytes.len() as u64) < needed {
        return Err("the clipboard bitmap is shorter than its header claims".to_string());
    }

    let mut rgba = vec![0u8; width as usize * height as usize * 4];
    for y in 0..height {
        // Bottom-up unless the height was negative.
        let source_row = if top_down { y } else { height - 1 - y };
        let row = offset + (source_row as usize * stride as usize);
        for x in 0..width {
            let pixel = row + (x as usize * (bits as usize / 8));
            let (b, g, r) = (bytes[pixel], bytes[pixel + 1], bytes[pixel + 2]);
            let target = (y as usize * width as usize + x as usize) * 4;
            rgba[target] = r;
            rgba[target + 1] = g;
            rgba[target + 2] = b;
            // `CF_DIB` 32-bit pixels are usually 0 in the alpha byte; treat the
            // bitmap as opaque rather than pasting a transparent square.
            rgba[target + 3] = 255;
        }
    }
    Ok((width, height, rgba))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Build a `CF_DIB` for a `width x height` 24-bit image with one solid colour.
    fn dib(width: u32, height: i32, rgb: (u8, u8, u8)) -> Vec<u8> {
        let mut header = vec![0u8; 40];
        header[0..4].copy_from_slice(&40u32.to_le_bytes());
        header[4..8].copy_from_slice(&(width as i32).to_le_bytes());
        header[8..12].copy_from_slice(&height.to_le_bytes());
        header[12..14].copy_from_slice(&1u16.to_le_bytes()); // planes
        header[14..16].copy_from_slice(&24u16.to_le_bytes());
        let stride = ((width * 24).div_ceil(32) * 4) as usize;
        let rows = height.unsigned_abs() as usize;
        let mut pixels = vec![0u8; stride * rows];
        // Bottom-up rows are reversed, but a solid colour hides the order.
        for row in pixels.chunks_mut(stride) {
            for x in 0..width as usize {
                row[x * 3] = rgb.2; // blue
                row[x * 3 + 1] = rgb.1; // green
                row[x * 3 + 2] = rgb.0; // red
            }
        }
        let mut out = header;
        out.extend_from_slice(&pixels);
        out
    }

    #[test]
    fn a_24_bit_bottom_up_bitmap_decodes_to_opaque_rgba() {
        let (width, height, rgba) = decode_dib(&dib(2, -2, (10, 20, 30))).expect("dib");
        assert_eq!((width, height), (2, 2));
        assert_eq!(rgba.len(), 2 * 2 * 4);
        assert_eq!(&rgba[..4], &[10, 20, 30, 255]);
    }

    #[test]
    fn a_top_down_bitmap_keeps_the_row_order() {
        // Two rows, top row red, bottom row blue, 1x2, top-down (negative height).
        let mut header = vec![0u8; 40];
        header[0..4].copy_from_slice(&40u32.to_le_bytes());
        header[4..8].copy_from_slice(&1i32.to_le_bytes());
        header[8..12].copy_from_slice(&(-2i32).to_le_bytes());
        header[12..14].copy_from_slice(&1u16.to_le_bytes());
        header[14..16].copy_from_slice(&24u16.to_le_bytes());
        let stride = 4usize;
        let mut pixels = vec![0u8; stride * 2];
        // Row 0 is red (B=0, G=0, R=255), row 1 is blue (B=255, G=0, R=0).
        pixels[0] = 0;
        pixels[1] = 0;
        pixels[2] = 255;
        pixels[4] = 255;
        pixels[5] = 0;
        pixels[6] = 0;
        let mut bytes = header;
        bytes.extend_from_slice(&pixels);

        let (_, height, rgba) = decode_dib(&bytes).expect("dib");
        assert_eq!(height, 2);
        assert_eq!(&rgba[..4], &[255, 0, 0, 255]);
    }

    #[test]
    fn unsupported_bitmaps_are_rejected_with_a_reason() {
        let mut header = vec![0u8; 40];
        header[0..4].copy_from_slice(&40u32.to_le_bytes());
        header[4..8].copy_from_slice(&4i32.to_le_bytes());
        header[8..12].copy_from_slice(&4i32.to_le_bytes());
        header[12..14].copy_from_slice(&1u16.to_le_bytes());
        header[14..16].copy_from_slice(&8u16.to_le_bytes()); // 8-bit palette
        let error = decode_dib(&header).expect_err("8-bit");
        assert!(error.contains("8-bit"), "{error}");

        header[14..16].copy_from_slice(&24u16.to_le_bytes());
        header[16..20].copy_from_slice(&3u32.to_le_bytes()); // BI_BITFIELDS
        let error = decode_dib(&header).expect_err("compressed");
        assert!(error.contains("uncompressed"), "{error}");
    }
}
