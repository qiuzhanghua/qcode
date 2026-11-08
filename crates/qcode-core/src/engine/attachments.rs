//! Persist the images a turn carries, before the turn is written to the log.
//!
//! The log keeps references only (ADR-0061 §1): the bytes land in the content
//! addressed store first, so a resumed turn can put them back.

use base64::Engine as _;
use base64::engine::general_purpose::STANDARD as BASE64;
use qcode_attachments::{
    AttachmentRef, AttachmentStore, FileRef, ImageMime, NewImage, NormalizationPolicy,
};

use crate::event::ContentBlock;
use crate::provider::Message;

use super::{AttachmentSource, EngineError};

/// Store every inline payload and hand back the references to log.
///
/// Payloads the caller already stored pass through untouched: the GUI saves a
/// picture the moment it is pasted, so the turn never copies it twice.
pub fn store_sources(
    store: &AttachmentStore,
    sources: &[AttachmentSource],
) -> Result<Vec<AttachmentRef>, EngineError> {
    let mut refs = Vec::with_capacity(sources.len());
    for source in sources {
        let attachment = match source {
            AttachmentSource::Stored(reference) => {
                refs.push(reference.clone());
                continue;
            }
            AttachmentSource::Inline(attachment) => attachment,
        };
        let mime = ImageMime::from_mime_str(&attachment.mime_type).ok_or_else(|| {
            EngineError::UnsupportedAttachment {
                mime: attachment.mime_type.clone(),
            }
        })?;
        let bytes = BASE64
            .decode(attachment.data_base64.as_bytes())
            .map_err(|_| EngineError::AttachmentDecode)?;
        // Normalize once, here: what the log points at is what the model sees
        // (ADR-0061 §5). Policy stays at its default for now.
        let image = store.put_image_normalized(
            NewImage {
                bytes: &bytes,
                mime,
                name: None,
            },
            &NormalizationPolicy::default(),
        )?;
        refs.push(AttachmentRef::Image(image));
    }
    Ok(refs)
}

/// The line a model reads in place of a stored file (ADR-0061 §7): enough to
/// find the bytes, and explicit about what happens when it cannot.
pub fn file_handle_text(store: &AttachmentStore, file: &FileRef) -> String {
    let sha = &file.id.to_hex()[..8];
    match store.file_path(file) {
        Ok(path) => format!(
            "[File {:?} ({} bytes, sha256:{sha}): read-only copy saved at {:?}. Read that path \
             with your file tools when its contents are needed; copy it to a writable location \
             before modifying it. When delegating file work, include this saved path in the \
             delegation prompt.]",
            file.name,
            file.bytes,
            path.display().to_string()
        ),
        Err(_) => format!(
            "[File {:?} ({} bytes, sha256:{sha}) was attached, but its saved copy is not readable \
             from this process. Report that limitation if its contents are needed; do not claim \
             to have read it.]",
            file.name, file.bytes
        ),
    }
}

/// Replace image blocks with a text placeholder when the model cannot see
/// images. The turn still runs: the model learns an image was attached, and a
/// later replay on a vision model can read the same ref out of the log
/// (ADR-0061 §8).
pub fn omit_images(messages: &mut [Message]) {
    for message in messages.iter_mut() {
        omit_images_in(&mut message.blocks);
    }
}

/// The same swap for one message's blocks.
pub fn omit_images_in(blocks: &mut [ContentBlock]) {
    {
        for block in blocks.iter_mut() {
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
