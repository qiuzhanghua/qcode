//! One normalization pass at write time (ADR-0061 §5).
//!
//! Decoding is delegated to `image`; encoding stays in-house where it can be
//! (PNG goes through `qcode-image`, JPEG through `image`). The policy is:
//!
//! * never keep a long edge above [`NormalizationPolicy::max_dimension`];
//! * never keep more than [`NormalizationPolicy::max_bytes`] of encoded bytes;
//! * keep alpha — a picture with transparency is never flattened to JPEG;
//! * a picture that already fits, with no EXIF rotation, passes through
//!   untouched, so a small pasted PNG is not needlessly re-encoded.

use std::io::Cursor;

use image::{DynamicImage, ImageDecoder, ImageEncoder, ImageFormat, ImageReader};

use crate::types::ImageMime;

/// ADR-0061 §5: long edge 2048, 2 MB, quality ladder 85 → 70 → 55.
#[derive(Debug, Clone, Copy)]
pub struct NormalizationPolicy {
    pub max_dimension: u32,
    pub max_bytes: u64,
    pub qualities: [u8; 3],
}

impl Default for NormalizationPolicy {
    fn default() -> Self {
        Self {
            max_dimension: 2048,
            max_bytes: 2 * 1024 * 1024,
            qualities: [85, 70, 55],
        }
    }
}

#[derive(Debug, thiserror::Error)]
pub enum NormalizeError {
    #[error("could not decode the image: {0}")]
    Decode(String),
    #[error("could not encode the image: {0}")]
    Encode(String),
    #[error("the image is still larger than {limit} bytes after scaling it down")]
    TooLarge { limit: u64 },
}

/// An encoded picture that is ready to be stored.
#[derive(Debug, Clone)]
pub struct NormalizedImage {
    pub bytes: Vec<u8>,
    pub mime: ImageMime,
    pub width: u32,
    pub height: u32,
    /// True when nothing was changed: the bytes are the caller's own.
    pub unchanged: bool,
}

/// Decode `bytes`, apply EXIF orientation, fit the policy, and re-encode.
///
/// `mime` is the declared type; the decoder is told to use it so a mislabelled
/// file fails here instead of producing a wrong `ImageRef`.
pub fn normalize(
    bytes: &[u8],
    mime: ImageMime,
    policy: &NormalizationPolicy,
) -> Result<NormalizedImage, NormalizeError> {
    let format = match mime {
        ImageMime::Png => ImageFormat::Png,
        ImageMime::Jpeg => ImageFormat::Jpeg,
        ImageMime::Webp => ImageFormat::WebP,
        ImageMime::Gif => ImageFormat::Gif,
    };
    let reader = ImageReader::with_format(Cursor::new(bytes), format);
    let mut decoder = reader
        .into_decoder()
        .map_err(|error| NormalizeError::Decode(error.to_string()))?;
    let orientation = decoder
        .orientation()
        .map_err(|error| NormalizeError::Decode(error.to_string()))?;
    let decoded = DynamicImage::from_decoder(decoder)
        .map_err(|error| NormalizeError::Decode(error.to_string()))?;

    let (mut width, mut height) = (decoded.width(), decoded.height());
    let upright = orientation == image::metadata::Orientation::NoTransforms;
    if upright
        && width <= policy.max_dimension
        && height <= policy.max_dimension
        && bytes.len() as u64 <= policy.max_bytes
    {
        return Ok(NormalizedImage {
            bytes: bytes.to_vec(),
            mime,
            width,
            height,
            unchanged: true,
        });
    }

    let mut picture = decoded;
    if !upright {
        picture.apply_orientation(orientation);
    }
    let rgba = picture.to_rgba8();
    width = rgba.width();
    height = rgba.height();

    let has_alpha = rgba.pixels().any(|pixel| pixel.0[3] != 255);
    let mut scale = fit_scale(width, height, policy.max_dimension);
    for _ in 0..8 {
        let (target_width, target_height) = scaled(width, height, scale);
        let frame = if scale < 1.0 {
            image::imageops::resize(
                &rgba,
                target_width,
                target_height,
                image::imageops::FilterType::Lanczos3,
            )
        } else {
            rgba.clone()
        };
        for candidate in encode_candidates(&frame, has_alpha, policy)? {
            if candidate.bytes.len() as u64 <= policy.max_bytes {
                return Ok(NormalizedImage {
                    bytes: candidate.bytes,
                    mime: candidate.mime,
                    width: frame.width(),
                    height: frame.height(),
                    unchanged: false,
                });
            }
        }
        scale *= 0.75;
    }
    Err(NormalizeError::TooLarge {
        limit: policy.max_bytes,
    })
}

struct Candidate {
    bytes: Vec<u8>,
    mime: ImageMime,
}

fn encode_candidates(
    frame: &image::RgbaImage,
    has_alpha: bool,
    policy: &NormalizationPolicy,
) -> Result<Vec<Candidate>, NormalizeError> {
    let mut out = Vec::with_capacity(4);
    // Transparency cannot survive JPEG, and a lossless PNG keeps its pixels.
    if has_alpha {
        let mut bytes = Vec::new();
        image::codecs::png::PngEncoder::new_with_quality(
            &mut bytes,
            image::codecs::png::CompressionType::Best,
            image::codecs::png::FilterType::Adaptive,
        )
        .write_image(
            frame.as_raw(),
            frame.width(),
            frame.height(),
            image::ExtendedColorType::Rgba8,
        )
        .map_err(|error| NormalizeError::Encode(error.to_string()))?;
        out.push(Candidate {
            bytes,
            mime: ImageMime::Png,
        });
        return Ok(out);
    }
    let rgb = image::RgbImage::from_fn(frame.width(), frame.height(), |x, y| {
        let pixel = frame.get_pixel(x, y);
        image::Rgb([pixel.0[0], pixel.0[1], pixel.0[2]])
    });
    for quality in policy.qualities {
        let mut bytes = Vec::new();
        let mut encoder = image::codecs::jpeg::JpegEncoder::new_with_quality(&mut bytes, quality);
        encoder
            .encode_image(&rgb)
            .map_err(|error| NormalizeError::Encode(error.to_string()))?;
        out.push(Candidate {
            bytes,
            mime: ImageMime::Jpeg,
        });
    }
    Ok(out)
}

fn fit_scale(width: u32, height: u32, max_dimension: u32) -> f32 {
    let longest = width.max(height) as f32;
    if longest <= max_dimension as f32 {
        1.0
    } else {
        max_dimension as f32 / longest
    }
}

fn scaled(width: u32, height: u32, scale: f32) -> (u32, u32) {
    (
        ((width as f32 * scale).round() as u32).max(1),
        ((height as f32 * scale).round() as u32).max(1),
    )
}
