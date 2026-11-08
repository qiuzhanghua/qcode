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

/// How much image payload one request may carry.
///
/// The counts are the wire budget, not the store budget: base64 inflates by a
/// third, so 20 MB here is roughly 15 MB of pictures. Both limits match what
/// the OpenAI-compatible APIs accept in one request.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub struct ImageBudget {
    /// Total base64 characters, as the request will actually carry them.
    pub max_bytes: usize,
    /// How many image blocks may appear in one request.
    pub max_images: usize,
}

impl Default for ImageBudget {
    fn default() -> Self {
        Self {
            max_bytes: 20 * 1024 * 1024,
            max_images: 20,
        }
    }
}

/// Replace the oldest pictures with a placeholder until the request fits
/// `budget`, and report how many were dropped.
///
/// The newest picture is the one the user just sent, so it is the last to go.
/// Nothing is written: the log keeps the reference, and the next request with
/// room for it shows the picture again (ADR-0061 §9).
pub fn offload_images(messages: &mut [Message], budget: ImageBudget) -> usize {
    let mut images = Vec::new();
    let mut bytes = 0usize;
    for (message, slot) in messages.iter().enumerate() {
        for (block, content) in slot.blocks.iter().enumerate() {
            if let ContentBlock::Image { data_base64, .. } = content {
                bytes += data_base64.len();
                images.push((message, block, data_base64.len()));
            }
        }
    }

    let mut dropped = 0;
    let mut remaining = images.len();
    for (message, block, size) in images {
        if remaining <= budget.max_images && bytes <= budget.max_bytes {
            break;
        }
        let ContentBlock::Image {
            mime_type,
            data_base64,
        } = &messages[message].blocks[block]
        else {
            continue;
        };
        let text = format!(
            "[image omitted to fit the request image limits; {mime_type}, {} bytes. \
             It stays in the session log, so it comes back once the request has room.]",
            data_base64.len() / 4 * 3
        );
        messages[message].blocks[block] = ContentBlock::Text { text };
        bytes -= size;
        remaining -= 1;
        dropped += 1;
    }
    dropped
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::provider::{Message, Role};

    fn image(payload: usize) -> ContentBlock {
        ContentBlock::Image {
            mime_type: "image/png".to_string(),
            data_base64: "A".repeat(payload),
        }
    }

    fn user(blocks: Vec<ContentBlock>) -> Message {
        Message {
            role: Role::User,
            blocks,
            tool_call_id: None,
        }
    }

    #[test]
    fn the_oldest_picture_is_the_first_to_go() {
        let mut messages = vec![
            user(vec![
                ContentBlock::Text {
                    text: "old".to_string(),
                },
                image(8),
            ]),
            user(vec![
                ContentBlock::Text {
                    text: "new".to_string(),
                },
                image(8),
            ]),
        ];
        let dropped = offload_images(
            &mut messages,
            ImageBudget {
                max_bytes: usize::MAX,
                max_images: 1,
            },
        );

        assert_eq!(dropped, 1);
        let ContentBlock::Text { text } = &messages[0].blocks[1] else {
            panic!("the oldest picture should be a placeholder");
        };
        assert!(
            text.contains("image omitted to fit the request image limits"),
            "{text}"
        );
        assert!(text.contains("image/png"), "{text}");
        // The newest picture is untouched.
        assert!(matches!(messages[1].blocks[1], ContentBlock::Image { .. }));
    }

    #[test]
    fn a_request_within_budget_is_left_alone() {
        let mut messages = vec![user(vec![image(8), image(8)])];
        let dropped = offload_images(&mut messages, ImageBudget::default());
        assert_eq!(dropped, 0);
        assert!(
            messages[0]
                .blocks
                .iter()
                .all(|block| matches!(block, ContentBlock::Image { .. }))
        );
    }

    #[test]
    fn the_byte_budget_can_drop_a_picture_too() {
        let mut messages = vec![user(vec![image(100), image(10)])];
        let dropped = offload_images(
            &mut messages,
            ImageBudget {
                max_bytes: 50,
                max_images: 20,
            },
        );

        assert_eq!(dropped, 1);
        assert!(matches!(messages[0].blocks[0], ContentBlock::Text { .. }));
        assert!(matches!(messages[0].blocks[1], ContentBlock::Image { .. }));
    }
}
