//! Attachments: references go into the session log, bytes go into a
//! content-addressed store. See ADR-0061.
//!
//! This crate is deliberately free of provider, engine and UI knowledge: it
//! answers "where do these bytes live" and "what does the log record", nothing
//! else.

pub mod dimensions;
pub mod id;
pub mod store;
pub mod types;

pub use dimensions::dimensions;
pub use id::AttachmentId;
pub use store::{
    AttachmentStore, MAX_FILE_BYTES, MAX_IMAGE_BYTES, MAX_NAME_BYTES, NewImage, StoreError,
    sanitize_file_name,
};
pub use types::{AttachmentRef, FileRef, ImageMime, ImageRef, OriginRef};
