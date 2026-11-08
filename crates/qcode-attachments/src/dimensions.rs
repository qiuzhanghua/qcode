//! Pixel dimensions straight from the container header.
//!
//! Decoding an image just to learn its size would pull a full codec into the
//! store crate; every format we accept states its dimensions in a fixed place
//! near the start of the file, so a few dozen lines is enough. Normalization
//! (ADR-0061 §5) decodes for real, and can refine these numbers afterwards.

use crate::types::ImageMime;

/// Width and height in pixels.
pub fn dimensions(mime: ImageMime, bytes: &[u8]) -> Option<(u32, u32)> {
    match mime {
        ImageMime::Png => png(bytes),
        ImageMime::Gif => gif(bytes),
        ImageMime::Jpeg => jpeg(bytes),
        ImageMime::Webp => webp(bytes),
    }
}

fn be16(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_be_bytes([slice[0], slice[1]]) as u32)
}

fn be32(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 4)?;
    Some(u32::from_be_bytes([slice[0], slice[1], slice[2], slice[3]]))
}

fn le16(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 2)?;
    Some(u16::from_le_bytes([slice[0], slice[1]]) as u32)
}

fn le24(bytes: &[u8], at: usize) -> Option<u32> {
    let slice = bytes.get(at..at + 3)?;
    Some(u32::from_le_bytes([slice[0], slice[1], slice[2], 0]))
}

fn png(bytes: &[u8]) -> Option<(u32, u32)> {
    // 8-byte signature, 4-byte chunk length, "IHDR", then width and height.
    if bytes.get(12..16)? != b"IHDR" {
        return None;
    }
    Some((be32(bytes, 16)?, be32(bytes, 20)?))
}

fn gif(bytes: &[u8]) -> Option<(u32, u32)> {
    Some((le16(bytes, 6)?, le16(bytes, 8)?))
}

fn jpeg(bytes: &[u8]) -> Option<(u32, u32)> {
    // Walk the marker segments until a start-of-frame: its payload is
    // precision, height, width, then the component count.
    let mut index = 2usize;
    while index + 3 < bytes.len() {
        if bytes[index] != 0xff {
            index += 1;
            continue;
        }
        let mut marker_at = index + 1;
        while bytes.get(marker_at) == Some(&0xff) {
            marker_at += 1;
        }
        let marker = *bytes.get(marker_at)?;
        index = marker_at + 1;
        match marker {
            // Standalone markers carry no length.
            0x01 | 0xd0..=0xd9 => continue,
            0xc0..=0xc3 | 0xc5..=0xc7 | 0xc9..=0xcb | 0xcd..=0xcf => {
                let height = be16(bytes, index + 3)?;
                let width = be16(bytes, index + 5)?;
                return Some((width, height));
            }
            _ => {}
        }
        let length = be16(bytes, index)? as usize;
        if length < 2 {
            return None;
        }
        index += length;
    }
    None
}

fn webp(bytes: &[u8]) -> Option<(u32, u32)> {
    let chunk = bytes.get(12..16)?;
    match chunk {
        b"VP8X" => {
            // Canvas size is stored minus one, 24 bits each, little endian.
            let width = le24(bytes, 24)? + 1;
            let height = le24(bytes, 27)? + 1;
            Some((width, height))
        }
        b"VP8 " => {
            // Lossy bitstream: 3-byte frame tag, then the start code.
            if bytes.get(23..26)? != [0x9d, 0x01, 0x2a] {
                return None;
            }
            let width = le16(bytes, 26)? & 0x3fff;
            let height = le16(bytes, 28)? & 0x3fff;
            Some((width, height))
        }
        b"VP8L" => {
            // Lossless bitstream: 0x2f signature, then packed 14-bit dims.
            if bytes.get(20) != Some(&0x2f) {
                return None;
            }
            let b1 = *bytes.get(21)? as u32;
            let b2 = *bytes.get(22)? as u32;
            let b3 = *bytes.get(23)? as u32;
            let b4 = *bytes.get(24)? as u32;
            let width = 1 + ((b1 | (b2 << 8)) & 0x3fff);
            let height = 1 + (((b2 >> 6) | (b3 << 2) | (b4 << 10)) & 0x3fff);
            Some((width, height))
        }
        _ => None,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn png_dimensions() {
        let mut bytes = b"\x89PNG\r\n\x1a\n\0\0\0\rIHDR".to_vec();
        bytes.extend_from_slice(&1568u32.to_be_bytes());
        bytes.extend_from_slice(&1024u32.to_be_bytes());
        assert_eq!(dimensions(ImageMime::Png, &bytes), Some((1568, 1024)));
    }

    #[test]
    fn gif_dimensions() {
        let mut bytes = b"GIF89a".to_vec();
        bytes.extend_from_slice(&320u16.to_le_bytes());
        bytes.extend_from_slice(&200u16.to_le_bytes());
        assert_eq!(dimensions(ImageMime::Gif, &bytes), Some((320, 200)));
    }

    #[test]
    fn jpeg_dimensions_skip_the_app_segment() {
        let mut bytes = vec![0xff, 0xd8];
        // APP0, length 6 (2 length bytes + 4 payload).
        bytes.extend_from_slice(&[0xff, 0xe0, 0x00, 0x06, 1, 2, 3, 4]);
        // SOF0, length 11: precision, height, width, 1 component.
        bytes.extend_from_slice(&[0xff, 0xc0, 0x00, 0x0b, 0x08]);
        bytes.extend_from_slice(&720u16.to_be_bytes());
        bytes.extend_from_slice(&1280u16.to_be_bytes());
        bytes.extend_from_slice(&[1, 1, 0x11, 0]);
        assert_eq!(dimensions(ImageMime::Jpeg, &bytes), Some((1280, 720)));
    }

    #[test]
    fn webp_lossy_dimensions() {
        let mut bytes = b"RIFF\0\0\0\0WEBPVP8 ".to_vec();
        bytes.extend_from_slice(&[0, 0, 0, 0]); // chunk size
        bytes.extend_from_slice(&[0, 0, 0]); // frame tag
        bytes.extend_from_slice(&[0x9d, 0x01, 0x2a]);
        // The top two bits are scaling hints; the dimensions sit in 14 bits.
        bytes.extend_from_slice(&(640u16 | 0xc000).to_le_bytes());
        bytes.extend_from_slice(&(480u16 | 0x4000).to_le_bytes());
        assert_eq!(dimensions(ImageMime::Webp, &bytes), Some((640, 480)));
    }

    #[test]
    fn webp_extended_dimensions() {
        let mut bytes = b"RIFF\0\0\0\0WEBPVP8X".to_vec();
        bytes.extend_from_slice(&[10, 0, 0, 0]); // chunk size
        bytes.extend_from_slice(&[0; 4]); // flags + reserved
        bytes.extend_from_slice(&[0x3f, 0x0c, 0x00]); // 3135 width - 1 = 3135? see below
        bytes.extend_from_slice(&[0xff, 0x07, 0x00]); // 2047 height - 1
        assert_eq!(dimensions(ImageMime::Webp, &bytes), Some((3136, 2048)));
    }

    #[test]
    fn truncated_headers_do_not_panic() {
        for mime in [
            ImageMime::Png,
            ImageMime::Gif,
            ImageMime::Jpeg,
            ImageMime::Webp,
        ] {
            assert_eq!(dimensions(mime, &[]), None);
            assert_eq!(dimensions(mime, b"\xff"), None);
        }
    }
}
