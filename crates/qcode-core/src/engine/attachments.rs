//! Persist the images a turn carries, before the turn is written to the log.
//!
//! The log keeps references only (ADR-0061 §1): the bytes land in the content
//! addressed store first, so a resumed turn can put them back.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use qcode_attachments::{AttachmentRef, AttachmentStore, ImageMime, NewImage};

use super::{Attachment, EngineError};

/// Store every inline image and hand back the references to log.
pub fn store_inline(
    store: &AttachmentStore,
    inline: &[Attachment],
) -> Result<Vec<AttachmentRef>, EngineError> {
    let mut refs = Vec::with_capacity(inline.len());
    for attachment in inline {
        let mime = ImageMime::from_mime_str(&attachment.mime_type).ok_or_else(|| {
            EngineError::UnsupportedAttachment {
                mime: attachment.mime_type.clone(),
            }
        })?;
        let bytes = BASE64
            .decode(attachment.data_base64.as_bytes())
            .map_err(|_| EngineError::AttachmentDecode)?;
        let image = store.put_image(NewImage {
            bytes: &bytes,
            mime,
            name: None,
        })?;
        refs.push(AttachmentRef::Image(image));
    }
    Ok(refs)
}
