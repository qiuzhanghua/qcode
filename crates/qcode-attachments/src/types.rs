//! The references that go into the session log.
//!
//! A reference is a pointer, never the bytes: the log stays small, diffable and
//! replayable without base64. See ADR-0061.

use serde::{Deserialize, Serialize};

use crate::id::AttachmentId;

/// What a user attached to a message.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(tag = "kind", rename_all = "snake_case")]
pub enum AttachmentRef {
    Image(ImageRef),
    File(FileRef),
}

impl AttachmentRef {
    pub fn id(&self) -> AttachmentId {
        match self {
            Self::Image(image) => image.id,
            Self::File(file) => file.id,
        }
    }

    pub fn bytes(&self) -> u64 {
        match self {
            Self::Image(image) => image.bytes,
            Self::File(file) => file.bytes,
        }
    }
}

/// An image in the content-addressed store.
///
/// `id` points at the normalized copy — the one that is sent to the model.
/// `origin` points at the bytes exactly as the user supplied them, and is only
/// present when normalization actually changed them.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct ImageRef {
    pub id: AttachmentId,
    pub mime: ImageMime,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub origin: Option<OriginRef>,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub name: Option<String>,
}

/// The untouched image, kept for "view original".
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct OriginRef {
    pub id: AttachmentId,
    pub bytes: u64,
    pub width: u32,
    pub height: u32,
}

/// A file stored byte for byte. The name is whatever the user attached; the
/// on-disk copy is sanitized.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
pub struct FileRef {
    pub id: AttachmentId,
    pub name: String,
    pub bytes: u64,
}

/// The image formats the store accepts.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash, Serialize, Deserialize)]
#[serde(rename_all = "snake_case")]
pub enum ImageMime {
    Png,
    Jpeg,
    Webp,
    Gif,
}

impl ImageMime {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Png => "image/png",
            Self::Jpeg => "image/jpeg",
            Self::Webp => "image/webp",
            Self::Gif => "image/gif",
        }
    }

    pub fn extension(self) -> &'static str {
        match self {
            Self::Png => "png",
            Self::Jpeg => "jpg",
            Self::Webp => "webp",
            Self::Gif => "gif",
        }
    }

    pub fn from_mime_str(text: &str) -> Option<Self> {
        match text.trim().to_ascii_lowercase().as_str() {
            "image/png" => Some(Self::Png),
            "image/jpeg" | "image/jpg" => Some(Self::Jpeg),
            "image/webp" => Some(Self::Webp),
            "image/gif" => Some(Self::Gif),
            _ => None,
        }
    }

    /// Identify the format from the bytes themselves.
    ///
    /// The declared mime type is only a hint from the clipboard; the bytes are
    /// what gets stored and later sent to a provider.
    pub fn sniff(bytes: &[u8]) -> Option<Self> {
        if bytes.starts_with(b"\x89PNG\r\n\x1a\n") {
            return Some(Self::Png);
        }
        if bytes.starts_with(&[0xff, 0xd8, 0xff]) {
            return Some(Self::Jpeg);
        }
        if bytes.starts_with(b"GIF87a") || bytes.starts_with(b"GIF89a") {
            return Some(Self::Gif);
        }
        if bytes.len() >= 12 && bytes.starts_with(b"RIFF") && &bytes[8..12] == b"WEBP" {
            return Some(Self::Webp);
        }
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn sniffs_each_format() {
        assert_eq!(
            ImageMime::sniff(b"\x89PNG\r\n\x1a\nrest"),
            Some(ImageMime::Png)
        );
        assert_eq!(
            ImageMime::sniff(&[0xff, 0xd8, 0xff, 0x00]),
            Some(ImageMime::Jpeg)
        );
        assert_eq!(ImageMime::sniff(b"GIF89a...."), Some(ImageMime::Gif));
        assert_eq!(
            ImageMime::sniff(b"RIFF\0\0\0\0WEBPVP8 "),
            Some(ImageMime::Webp)
        );
        assert_eq!(ImageMime::sniff(b"not an image"), None);
    }

    #[test]
    fn mime_strings_round_trip() {
        for mime in [
            ImageMime::Png,
            ImageMime::Jpeg,
            ImageMime::Webp,
            ImageMime::Gif,
        ] {
            assert_eq!(ImageMime::from_mime_str(mime.as_str()), Some(mime));
        }
        assert_eq!(ImageMime::from_mime_str("image/jpg"), Some(ImageMime::Jpeg));
        assert_eq!(ImageMime::from_mime_str("text/plain"), None);
    }

    #[test]
    fn attachment_ref_wire_shape() {
        let image = AttachmentRef::Image(ImageRef {
            id: AttachmentId::from_bytes([7; 32]),
            mime: ImageMime::Png,
            bytes: 1024,
            width: 100,
            height: 50,
            origin: None,
            name: Some("shot.png".into()),
        });
        let json = serde_json::to_value(&image).unwrap();
        assert_eq!(json["kind"], "image");
        assert_eq!(json["mime"], "png");
        assert_eq!(json["width"], 100);
        assert!(json.get("origin").is_none(), "empty origin is omitted");

        let file = AttachmentRef::File(FileRef {
            id: AttachmentId::from_bytes([8; 32]),
            name: "notes.md".into(),
            bytes: 12,
        });
        let json = serde_json::to_value(&file).unwrap();
        assert_eq!(json["kind"], "file");

        assert_eq!(serde_json::from_value::<AttachmentRef>(json).unwrap(), file);
    }
}
