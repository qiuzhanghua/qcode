//! Minimal PNG encoder for pasted images.
//!
//! The offline build has no `png`/`flate2` crate available, so this writes the
//! smallest valid file it can: RGBA8, one `IDAT`, and a zlib stream made of
//! *stored* (uncompressed) deflate blocks. That is bigger than a compressed PNG
//! but always correct, and a pasted screenshot is a few hundred kilobytes.

/// Encode 8-bit RGBA pixels as a PNG.
///
/// # Panics
/// If `pixels` is not exactly `width * height * 4` bytes.
pub fn encode_rgba(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    assert_eq!(
        pixels.len(),
        width as usize * height as usize * 4,
        "pixel buffer does not match the dimensions"
    );

    // Raw scanlines: one filter byte (0 = none) followed by the row.
    let stride = width as usize * 4;
    let mut raw = Vec::with_capacity((stride + 1) * height as usize);
    for row in 0..height as usize {
        raw.push(0);
        raw.extend_from_slice(&pixels[row * stride..(row + 1) * stride]);
    }

    let mut out = Vec::new();
    out.extend_from_slice(&[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

    let mut ihdr = Vec::with_capacity(13);
    ihdr.extend_from_slice(&width.to_be_bytes());
    ihdr.extend_from_slice(&height.to_be_bytes());
    ihdr.extend_from_slice(&[8, 6, 0, 0, 0]); // 8-bit, RGBA, deflate, no filter, no interlace
    chunk(&mut out, b"IHDR", &ihdr);

    chunk(&mut out, b"IDAT", &zlib_stored(&raw));
    chunk(&mut out, b"IEND", &[]);
    out
}

/// A zlib stream whose deflate body is nothing but stored blocks.
fn zlib_stored(data: &[u8]) -> Vec<u8> {
    let mut out = vec![0x78, 0x01]; // deflate, 32K window, no preset dictionary
    let mut remaining = data;
    loop {
        let take = remaining.len().min(65_535);
        let last = take == remaining.len();
        out.push(u8::from(last));
        out.extend_from_slice(&(take as u16).to_le_bytes());
        out.extend_from_slice(&(!(take as u16)).to_le_bytes());
        out.extend_from_slice(&remaining[..take]);
        remaining = &remaining[take..];
        if last {
            break;
        }
    }
    out.extend_from_slice(&adler32(data).to_be_bytes());
    out
}

fn chunk(out: &mut Vec<u8>, kind: &[u8; 4], data: &[u8]) {
    out.extend_from_slice(&(data.len() as u32).to_be_bytes());
    let start = out.len();
    out.extend_from_slice(kind);
    out.extend_from_slice(data);
    let crc = crc32(&out[start..]);
    out.extend_from_slice(&crc.to_be_bytes());
}

/// CRC-32 (IEEE 802.3), the polynomial PNG uses.
fn crc32(bytes: &[u8]) -> u32 {
    let mut crc = 0xffff_ffffu32;
    for byte in bytes {
        crc ^= u32::from(*byte);
        for _ in 0..8 {
            let mask = (crc & 1).wrapping_neg();
            crc = (crc >> 1) ^ (0xedb8_8320 & mask);
        }
    }
    !crc
}

/// Adler-32, the checksum zlib puts at the end of a stream.
fn adler32(bytes: &[u8]) -> u32 {
    let (mut a, mut b) = (1u32, 0u32);
    for byte in bytes {
        a = (a + u32::from(*byte)) % 65_521;
        b = (b + a) % 65_521;
    }
    (b << 16) | a
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_checksums_match_the_published_vectors() {
        assert_eq!(crc32(b"123456789"), 0xcbf4_3926);
        assert_eq!(adler32(b"Wikipedia"), 0x11e6_0398);
    }

    #[test]
    fn a_png_has_the_signature_the_header_and_an_end_chunk() {
        let pixels = [
            255, 0, 0, 255, // red
            0, 255, 0, 255, // green
            0, 0, 255, 255, // blue
            255, 255, 255, 255, // white
        ];
        let png = encode_rgba(2, 2, &pixels);
        assert_eq!(&png[..8], &[0x89, b'P', b'N', b'G', 0x0d, 0x0a, 0x1a, 0x0a]);

        // IHDR is the first chunk: width and height are big-endian.
        let kind = &png[12..16];
        assert_eq!(kind, b"IHDR");
        assert_eq!(u32::from_be_bytes([png[16], png[17], png[18], png[19]]), 2);
        assert_eq!(u32::from_be_bytes([png[20], png[21], png[22], png[23]]), 2);
        assert_eq!(&png[24..29], &[8, 6, 0, 0, 0]);

        assert!(png.ends_with(&[b'I', b'E', b'N', b'D', 0xae, 0x42, 0x60, 0x82]));
    }

    #[test]
    fn the_idat_stream_starts_with_a_zlib_header_and_carries_the_raw_rows() {
        let png = encode_rgba(1, 1, &[1, 2, 3, 255]);
        let idat = png
            .windows(4)
            .position(|window| window == b"IDAT")
            .expect("IDAT");
        // Data starts right after the chunk type.
        let stream = &png[idat + 4..];
        assert_eq!(&stream[..2], &[0x78, 0x01]);
        assert_eq!(stream[2], 1, "single stored block, final");
        // 1 filter byte + 4 pixel bytes.
        assert_eq!(u16::from_le_bytes([stream[3], stream[4]]), 5);
        assert_eq!(&stream[7..12], &[0, 1, 2, 3, 255]);
    }
}
