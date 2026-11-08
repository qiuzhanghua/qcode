//! ADR-0061 §5: one normalization pass at write time.

use image::{ExtendedColorType, ImageEncoder};

fn encode_png(width: u32, height: u32, pixels: &[u8]) -> Vec<u8> {
    let mut bytes = Vec::new();
    image::codecs::png::PngEncoder::new(&mut bytes)
        .write_image(pixels, width, height, ExtendedColorType::Rgba8)
        .unwrap();
    bytes
}

use qcode_attachments::{
    AttachmentStore, ImageMime, NewImage, NormalizationPolicy, normalize, store::StoreError,
};

/// An opaque gradient, big enough that the policy has to shrink it.
fn big_opaque_png(width: u32, height: u32) -> Vec<u8> {
    // Blocky on purpose: the PNG has to stay under the 8 MB input cap while the
    // long edge is still above the 2048 policy limit.
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for x in 0..width {
        let block = ((x / 64) % 3) as u8 * 80;
        for _ in 0..height {
            pixels.extend_from_slice(&[block, block / 2, 200, 255]);
        }
    }
    encode_png(width, height, &pixels)
}

fn big_alpha_png(width: u32, height: u32) -> Vec<u8> {
    let mut pixels = Vec::with_capacity((width * height * 4) as usize);
    for y in 0..height {
        for x in 0..width {
            let alpha = if x < width / 2 { 128 } else { 255 };
            let block = ((y / 64) % 3) as u8 * 80;
            pixels.extend_from_slice(&[block, block / 2, 200, alpha]);
        }
    }
    encode_png(width, height, &pixels)
}

#[test]
fn a_small_opaque_png_passes_through_untouched() {
    let bytes = big_opaque_png(2, 1);
    let normalized = normalize(&bytes, ImageMime::Png, &NormalizationPolicy::default()).unwrap();
    assert!(normalized.unchanged);
    assert_eq!(normalized.mime, ImageMime::Png);
    assert_eq!(normalized.bytes, bytes);
}

#[test]
fn a_big_opaque_png_is_shrunk_and_re_encoded_as_jpeg() {
    let bytes = big_opaque_png(2400, 1600);
    let normalized = normalize(&bytes, ImageMime::Png, &NormalizationPolicy::default()).unwrap();
    assert!(!normalized.unchanged);
    assert_eq!(normalized.mime, ImageMime::Jpeg);
    assert_eq!(normalized.width.max(normalized.height), 2048);
    assert!(normalized.bytes.len() as u64 <= 2 * 1024 * 1024);
    // The encoded copy still decodes to a picture of the promised size.
    let picture = image::load_from_memory(&normalized.bytes).unwrap();
    assert_eq!(
        (picture.width(), picture.height()),
        (normalized.width, normalized.height)
    );
}

#[test]
fn transparency_survives_normalization() {
    let bytes = big_alpha_png(2400, 1600);
    let normalized = normalize(&bytes, ImageMime::Png, &NormalizationPolicy::default()).unwrap();
    assert_eq!(normalized.mime, ImageMime::Png);
    let picture = image::load_from_memory(&normalized.bytes)
        .unwrap()
        .to_rgba8();
    assert!(picture.pixels().any(|pixel| pixel.0[3] != 255));
}

#[test]
fn the_store_keeps_both_the_normalized_copy_and_the_original() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::open(dir.path()).unwrap();
    let bytes = big_opaque_png(2400, 1600);
    let image = store
        .put_image_normalized(
            NewImage {
                bytes: &bytes,
                mime: ImageMime::Png,
                name: Some("big.png"),
            },
            &NormalizationPolicy::default(),
        )
        .unwrap();

    assert_eq!(image.mime, ImageMime::Jpeg);
    assert!(image.bytes <= 2 * 1024 * 1024);
    let origin = image.origin.as_ref().expect("the original was rewritten");
    assert_eq!(origin.bytes, bytes.len() as u64);
    assert_eq!((origin.width, origin.height), (2400, 1600));

    let normalized = store.read_image(&image).unwrap();
    assert_eq!(normalized.len() as u64, image.bytes);
    assert_ne!(normalized, bytes);
    let picture = image::load_from_memory(&normalized).unwrap();
    assert_eq!(
        (picture.width(), picture.height()),
        (image.width, image.height)
    );
    assert_eq!(store.read_origin(origin).unwrap(), bytes);
    assert_eq!(image.name.as_deref(), Some("big.png"));
}

#[test]
fn a_mislabelled_image_is_refused() {
    let dir = tempfile::tempdir().unwrap();
    let store = AttachmentStore::open(dir.path()).unwrap();
    let bytes = big_opaque_png(4, 4);
    let error = store
        .put_image_normalized(
            NewImage {
                bytes: &bytes,
                mime: ImageMime::Jpeg,
                name: None,
            },
            &NormalizationPolicy::default(),
        )
        .unwrap_err();
    assert!(matches!(error, StoreError::MimeMismatch { .. }));
}
