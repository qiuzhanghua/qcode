//! Persist the images a turn carries, before the turn is written to the log.
//!
//! The log keeps references only (ADR-0061 §1): the bytes land in the content
//! addressed store first, so a resumed turn can put them back.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use qcode_attachments::{AttachmentRef, AttachmentStore, ImageMime, NewImage};

use crate::event::ContentBlock;
use crate::provider::Message;

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

/// Replace image blocks with a text placeholder when the model cannot see
/// images. The turn still runs: the model learns an image was attached, and a
/// later replay on a vision model can read the same ref out of the log
/// (ADR-0061 §8).
pub fn omit_images(messages: &mut [Message]) {
    for message in messages.iter_mut() {
        for block in message.blocks.iter_mut() {
            let ContentBlock::Image {
                mime_type,
                data_base64,
            } = block
            else {
                continue;
            };
            // base64 is 4 bytes per 3 bytes of payload, so this is the size the
            // user actually attached, give or take padding.
            let bytes = data_base64.len() / 4 * 3;
            let text = format!(
                "[image omitted: this model accepts text only; {mime_type}, {bytes} bytes. \
                 The image stays in the session log, so a model that accepts images can \
                 replay it.]"
            );
            *block = ContentBlock::Text { text };
        }
    }
}
