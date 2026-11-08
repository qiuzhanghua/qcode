//! Pending image attachments in the composer.
//!
//! Pasting, picking or dropping an image writes it into the content-addressed
//! store right away (ADR-0061 §4: store first, send later). The composer then
//! draws a thumbnail from the stored object instead of decoding anything
//! itself: GPUI's `img()` already takes a path and does the decode.

use std::path::{Path, PathBuf};

use qcode_attachments::{AttachmentStore, ImageMime, NewImage};
use qcode_core::engine::Attachment;

/// An image waiting in the composer, already stored.
#[derive(Debug)]
pub(crate) struct PendingAttachment {
    /// What the turn sends. The engine stores these bytes again and hits the
    /// same object (same bytes, same sha), so sending costs one hash.
    pub(crate) attachment: Attachment,
    /// The stored object, rendered as the thumbnail.
    pub(crate) preview: PathBuf,
    /// File name, or `pasted image` for the clipboard.
    pub(crate) label: String,
}

/// Store `bytes` and return the composer item.
///
/// `label` is the file name, or something like `pasted image` for the
/// clipboard. The error text is already user-facing: the caller shows it as a
/// notice instead of a thumbnail.
pub(crate) fn attach_image(
    data_dir: &Path,
    bytes: &[u8],
    label: impl Into<String>,
) -> Result<PendingAttachment, String> {
    let label = label.into();
    let Some(mime) = ImageMime::sniff(bytes) else {
        return Err(format!(
            "{label}: 不是支持的图片格式（png / jpeg / webp / gif）"
        ));
    };
    let store = AttachmentStore::open(data_dir).map_err(|error| error.to_string())?;
    let image = store
        .put_image(NewImage {
            bytes,
            mime,
            name: Some(&label),
        })
        .map_err(|error| error.to_string())?;
    use base64::Engine as _;
    Ok(PendingAttachment {
        attachment: Attachment {
            mime_type: image.mime.as_str().to_string(),
            data_base64: base64::engine::general_purpose::STANDARD.encode(bytes),
        },
        preview: store.image_path(&image),
        label,
    })
}

/// Read `path` and attach it if it is an image, else hand the path back so the
/// caller can keep its "attach as a file reference" behaviour.
pub(crate) fn attach_image_path(
    data_dir: &Path,
    path: &Path,
) -> Result<Option<PendingAttachment>, String> {
    if !is_image_path(path) {
        return Ok(None);
    }
    let label = path
        .file_name()
        .and_then(|name| name.to_str())
        .unwrap_or("image")
        .to_string();
    let bytes = std::fs::read(path).map_err(|error| format!("{}: {error}", path.display()))?;
    attach_image(data_dir, &bytes, label).map(Some)
}

/// Whether the extension is one of the four formats the store accepts.
pub(crate) fn is_image_path(path: &Path) -> bool {
    path.extension()
        .and_then(|ext| ext.to_str())
        .map(|ext| ext.to_ascii_lowercase())
        .is_some_and(|ext| matches!(ext.as_str(), "png" | "jpg" | "jpeg" | "webp" | "gif"))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A 1×1 PNG.
    fn png() -> Vec<u8> {
        use base64::Engine as _;
        base64::engine::general_purpose::STANDARD
            .decode("iVBORw0KGgoAAAANSUhEUgAAAAEAAAABCAYAAAAfFcSJAAAADUlEQVR42mP8z8BQDwAEhQGAhKmMIQAAAABJRU5ErkJggg==")
            .expect("fixture")
    }

    #[test]
    fn attaching_stores_the_object_and_hands_back_its_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let pending = attach_image(dir.path(), &png(), "shot.png").expect("attach");
        assert_eq!(pending.attachment.mime_type, "image/png");
        assert_eq!(pending.label, "shot.png");
        assert!(pending.preview.exists(), "the thumbnail path must exist");
        assert!(pending.preview.starts_with(dir.path().join("attachments")));

        // Storing the same bytes again reuses the object.
        let again = attach_image(dir.path(), &png(), "copy.png").expect("attach again");
        assert_eq!(again.preview, pending.preview);
    }

    #[test]
    fn non_images_are_refused_with_a_readable_message() {
        let dir = tempfile::tempdir().expect("tempdir");
        let error = attach_image(dir.path(), b"plain text", "notes.txt").expect_err("refused");
        assert!(error.contains("notes.txt"), "{error}");
        assert!(error.contains("png"), "{error}");
    }

    #[test]
    fn only_four_extensions_count_as_images() {
        assert!(is_image_path(Path::new("a.PNG")));
        assert!(is_image_path(Path::new("a.jpeg")));
        assert!(is_image_path(Path::new("a.webp")));
        assert!(is_image_path(Path::new("a.gif")));
        assert!(!is_image_path(Path::new("a.pdf")));
        assert!(!is_image_path(Path::new("a.png.txt")));
        assert!(!is_image_path(Path::new("noext")));
    }

    #[test]
    fn a_non_image_path_is_left_to_the_file_reference_path() {
        let dir = tempfile::tempdir().expect("tempdir");
        let path = dir.path().join("report.pdf");
        std::fs::write(&path, b"%PDF-1.4").expect("write");
        assert!(
            attach_image_path(dir.path(), &path)
                .expect("no error")
                .is_none()
        );
    }
}
