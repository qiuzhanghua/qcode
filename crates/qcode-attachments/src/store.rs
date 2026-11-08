//! Content-addressed storage for attachments (ADR-0061 §2).
//!
//! Layout under `<data dir>/attachments/v1`:
//!
//! ```text
//! objects/<sha[..2]>/<sha256>       normalized image: the bytes sent to the model
//! originals/<sha[..2]>/<sha256>     image exactly as supplied
//! files/<sha[..2]>/<sha256>/<name>  file, byte for byte
//! tmp/                              files being written
//! ```
//!
//! Every write goes to `tmp/`, is fsynced, and is then renamed into place, so a
//! crash leaves either a complete object or nothing. Objects are set read-only;
//! content addressing makes a second write of the same bytes a no-op.

use std::fs::{self, File, OpenOptions};
use std::io::{self, Read, Write};
use std::path::{Path, PathBuf};
use std::time::{SystemTime, UNIX_EPOCH};

use sha2::{Digest, Sha256};

use crate::id::AttachmentId;
use crate::types::{FileRef, ImageMime, ImageRef, OriginRef};

/// Nothing larger than this is copied into the store (ADR-0061 §7).
pub const MAX_FILE_BYTES: u64 = 64 * 1024 * 1024;
/// Images are decoded and normalized before they get here, so this is a backstop.
pub const MAX_IMAGE_BYTES: u64 = 8 * 1024 * 1024;
/// Longest file name in bytes; longer names are truncated on a char boundary.
pub const MAX_NAME_BYTES: usize = 255;

const COPY_BUFFER: usize = 64 * 1024;

#[derive(Debug, thiserror::Error)]
pub enum StoreError {
    #[error("i/o error at {path}: {source}")]
    Io {
        path: PathBuf,
        #[source]
        source: io::Error,
    },
    #[error("attachment is larger than the {limit} byte limit")]
    TooLarge { limit: u64 },
    #[error("no attachment at {path}")]
    Missing { path: PathBuf },
    #[error("declared {declared:?} but the bytes are {found:?}")]
    MimeMismatch {
        declared: ImageMime,
        found: Option<ImageMime>,
    },
    #[error("could not read the dimensions of a {mime:?} image")]
    Undecodable { mime: ImageMime },
}

/// An image handed to [`AttachmentStore::put_image`].
#[derive(Debug, Clone, Copy)]
pub struct NewImage<'a> {
    pub bytes: &'a [u8],
    pub mime: ImageMime,
    pub name: Option<&'a str>,
}

/// The store rooted at one data directory.
#[derive(Debug, Clone)]
pub struct AttachmentStore {
    root: PathBuf,
}

impl AttachmentStore {
    /// Open (creating if needed) the store under `<data_dir>/attachments/v1`.
    ///
    /// Leftover `tmp/` files from a previous crash are removed: nothing in there
    /// is referenced by a log yet.
    pub fn open(data_dir: impl AsRef<Path>) -> Result<Self, StoreError> {
        let root = data_dir.as_ref().join("attachments").join("v1");
        for dir in ["objects", "originals", "files", "tmp"] {
            let path = root.join(dir);
            fs::create_dir_all(&path).map_err(|source| io_error(&path, source))?;
        }
        let store = Self { root };
        store.clear_tmp()?;
        Ok(store)
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    /// Store a file byte for byte, rejecting anything over [`MAX_FILE_BYTES`].
    pub fn put_file<R: Read>(&self, name: &str, reader: R) -> Result<FileRef, StoreError> {
        self.put_file_capped(name, reader, MAX_FILE_BYTES)
    }

    /// [`Self::put_file`] with an explicit limit, so tests do not need 64 MB.
    pub fn put_file_capped<R: Read>(
        &self,
        name: &str,
        mut reader: R,
        limit: u64,
    ) -> Result<FileRef, StoreError> {
        let leaf = sanitize_file_name(name);
        let mut tmp = TempFile::create(&self.root)?;
        let mut hasher = Sha256::new();
        let mut total: u64 = 0;
        let mut buffer = vec![0u8; COPY_BUFFER];
        loop {
            let read = reader
                .read(&mut buffer)
                .map_err(|source| io_error(&tmp.path, source))?;
            if read == 0 {
                break;
            }
            total += read as u64;
            if total > limit {
                return Err(StoreError::TooLarge { limit });
            }
            hasher.update(&buffer[..read]);
            tmp.write_all(&buffer[..read])?;
        }

        let id = AttachmentId::from_bytes(hasher.finalize().into());
        let dir = self.file_dir(id);
        let target = dir.join(&leaf);
        let reference = FileRef {
            id,
            name: name.to_string(),
            bytes: total,
        };
        if target.exists() {
            return Ok(reference);
        }

        fs::create_dir_all(&dir).map_err(|source| io_error(&dir, source))?;
        tmp.commit_to(&target)?;
        set_read_only(&target)?;
        fsync_dir(&dir)?;
        Ok(reference)
    }

    /// Store image bytes as-is.
    ///
    /// Normalization does not live here yet (ADR-0061 §5): the caller decodes,
    /// resizes and re-encodes, then hands over the final bytes plus dimensions.
    pub fn put_image(&self, image: NewImage<'_>) -> Result<ImageRef, StoreError> {
        let size = image.bytes.len() as u64;
        if size > MAX_IMAGE_BYTES {
            return Err(StoreError::TooLarge {
                limit: MAX_IMAGE_BYTES,
            });
        }
        let found = ImageMime::sniff(image.bytes);
        if found != Some(image.mime) {
            return Err(StoreError::MimeMismatch {
                declared: image.mime,
                found,
            });
        }

        let (width, height) = crate::dimensions::dimensions(image.mime, image.bytes)
            .ok_or(StoreError::Undecodable { mime: image.mime })?;

        let id = AttachmentId::from_bytes(Sha256::digest(image.bytes).into());
        let target = self.object_path("objects", id);
        if !target.exists() {
            let mut tmp = TempFile::create(&self.root)?;
            tmp.write_all(image.bytes)?;
            let parent = target.parent().expect("object path has a shard directory");
            fs::create_dir_all(parent).map_err(|source| io_error(parent, source))?;
            tmp.commit_to(&target)?;
            set_read_only(&target)?;
            fsync_dir(parent)?;
        }

        Ok(ImageRef {
            id,
            mime: image.mime,
            bytes: size,
            width,
            height,
            origin: None,
            name: image.name.map(str::to_string),
        })
    }

    /// Where the normalized image lives. Not checked for existence.
    pub fn image_path(&self, image: &ImageRef) -> PathBuf {
        self.object_path("objects", image.id)
    }

    /// Where the uploaded copy of `id` lives, derived from the data directory
    /// alone. Rendering uses this: it must not open (and tidy) the store.
    pub fn object_path_in(data_dir: impl AsRef<Path>, id: AttachmentId) -> PathBuf {
        data_dir
            .as_ref()
            .join("attachments")
            .join("v1")
            .join("objects")
            .join(id.shard())
            .join(id.to_hex())
    }

    /// Where the untouched original lives. Not checked for existence.
    pub fn origin_path(&self, origin: &OriginRef) -> PathBuf {
        self.object_path("originals", origin.id)
    }

    /// Where a stored file lives; verifies it is still there.
    pub fn file_path(&self, file: &FileRef) -> Result<PathBuf, StoreError> {
        let path = self.file_dir(file.id).join(sanitize_file_name(&file.name));
        ensure_present(&path)?;
        Ok(path)
    }

    pub fn read_image(&self, image: &ImageRef) -> Result<Vec<u8>, StoreError> {
        read_bytes(&self.image_path(image))
    }

    pub fn read_origin(&self, origin: &OriginRef) -> Result<Vec<u8>, StoreError> {
        read_bytes(&self.origin_path(origin))
    }

    pub fn read_file(&self, file: &FileRef) -> Result<Vec<u8>, StoreError> {
        read_bytes(&self.file_path(file)?)
    }

    fn object_path(&self, dir: &str, id: AttachmentId) -> PathBuf {
        self.root.join(dir).join(id.shard()).join(id.to_hex())
    }

    fn file_dir(&self, id: AttachmentId) -> PathBuf {
        self.root.join("files").join(id.shard()).join(id.to_hex())
    }

    fn clear_tmp(&self) -> Result<(), StoreError> {
        let dir = self.root.join("tmp");
        let entries = fs::read_dir(&dir).map_err(|source| io_error(&dir, source))?;
        for entry in entries {
            let entry = entry.map_err(|source| io_error(&dir, source))?;
            let path = entry.path();
            // A leftover temp file is never referenced; ignore failures (a
            // concurrently running writer may have just renamed it away).
            let _ = fs::remove_file(&path);
        }
        Ok(())
    }
}

/// Reduce a user-supplied file name to a single safe path component.
///
/// Path separators are stripped (both kinds — a Windows path pasted on macOS
/// must not become a directory), control characters dropped, characters
/// Windows refuses replaced, reserved device names prefixed, and the result
/// truncated to [`MAX_NAME_BYTES`] on a char boundary.
pub fn sanitize_file_name(name: &str) -> String {
    const RESERVED: [&str; 22] = [
        "CON", "PRN", "AUX", "NUL", "COM1", "COM2", "COM3", "COM4", "COM5", "COM6", "COM7", "COM8",
        "COM9", "LPT1", "LPT2", "LPT3", "LPT4", "LPT5", "LPT6", "LPT7", "LPT8", "LPT9",
    ];

    // Trailing separators make the last component empty; drop them first so
    // "a/b/" is still "b" rather than "file".
    let trimmed_end = name.trim_end_matches(['/', '\\']);
    let base = trimmed_end
        .rsplit(['/', '\\'])
        .next()
        .unwrap_or(trimmed_end);
    let mut cleaned = String::with_capacity(base.len());
    for ch in base.chars() {
        if ch.is_control() {
            continue;
        }
        cleaned.push(match ch {
            '<' | '>' | ':' | '"' | '|' | '?' | '*' => '_',
            other => other,
        });
    }

    let trimmed = cleaned.trim();
    let mut leaf = if trimmed.is_empty() || trimmed == "." || trimmed == ".." {
        "file".to_string()
    } else {
        trimmed.to_string()
    };

    let stem = leaf
        .split('.')
        .next()
        .unwrap_or_default()
        .to_ascii_uppercase();
    if RESERVED.contains(&stem.as_str()) {
        leaf.insert(0, '_');
    }

    truncate_bytes(leaf, MAX_NAME_BYTES)
}

fn truncate_bytes(text: String, max: usize) -> String {
    if text.len() <= max {
        return text;
    }
    let mut end = 0;
    for (index, ch) in text.char_indices() {
        if index + ch.len_utf8() > max {
            break;
        }
        end = index + ch.len_utf8();
    }
    text[..end].to_string()
}

fn ensure_present(path: &Path) -> Result<(), StoreError> {
    match fs::metadata(path) {
        Ok(_) => Ok(()),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Err(StoreError::Missing {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(io_error(path, source)),
    }
}

fn read_bytes(path: &Path) -> Result<Vec<u8>, StoreError> {
    match fs::read(path) {
        Ok(bytes) => Ok(bytes),
        Err(source) if source.kind() == io::ErrorKind::NotFound => Err(StoreError::Missing {
            path: path.to_path_buf(),
        }),
        Err(source) => Err(io_error(path, source)),
    }
}

fn set_read_only(path: &Path) -> Result<(), StoreError> {
    let mut permissions = fs::metadata(path)
        .map_err(|source| io_error(path, source))?
        .permissions();
    permissions.set_readonly(true);
    fs::set_permissions(path, permissions).map_err(|source| io_error(path, source))
}

fn fsync_dir(dir: &Path) -> Result<(), StoreError> {
    #[cfg(unix)]
    {
        File::open(dir)
            .map_err(|source| io_error(dir, source))?
            .sync_all()
            .map_err(|source| io_error(dir, source))?;
    }
    #[cfg(not(unix))]
    {
        let _ = dir;
    }
    Ok(())
}

fn io_error(path: impl AsRef<Path>, source: io::Error) -> StoreError {
    StoreError::Io {
        path: path.as_ref().to_path_buf(),
        source,
    }
}

fn nanos() -> u128 {
    SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .map(|elapsed| elapsed.as_nanos())
        .unwrap_or_default()
}

/// A file in `tmp/` that deletes itself unless committed.
struct TempFile {
    path: PathBuf,
    file: Option<File>,
    committed: bool,
}

impl TempFile {
    fn create(root: &Path) -> Result<Self, StoreError> {
        let dir = root.join("tmp");
        for attempt in 0..64u32 {
            let path = dir.join(format!("{}-{}-{}", std::process::id(), nanos(), attempt));
            match OpenOptions::new().write(true).create_new(true).open(&path) {
                Ok(file) => {
                    return Ok(Self {
                        path,
                        file: Some(file),
                        committed: false,
                    });
                }
                Err(source) if source.kind() == io::ErrorKind::AlreadyExists => continue,
                Err(source) => return Err(io_error(&path, source)),
            }
        }
        Err(io_error(
            dir,
            io::Error::new(
                io::ErrorKind::AlreadyExists,
                "too many temp file collisions",
            ),
        ))
    }

    fn write_all(&mut self, bytes: &[u8]) -> Result<(), StoreError> {
        let file = self.file.as_mut().expect("temp file is open");
        file.write_all(bytes)
            .map_err(|source| io_error(&self.path, source))
    }

    fn commit_to(mut self, target: &Path) -> Result<(), StoreError> {
        let file = self.file.take().expect("temp file is open");
        file.sync_all()
            .map_err(|source| io_error(&self.path, source))?;
        drop(file);
        fs::rename(&self.path, target).map_err(|source| io_error(target, source))?;
        self.committed = true;
        Ok(())
    }
}

impl Drop for TempFile {
    fn drop(&mut self) {
        if !self.committed {
            let _ = fs::remove_file(&self.path);
        }
    }
}
