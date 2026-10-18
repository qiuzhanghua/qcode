//! Workspace instructions (`AGENTS.md`).
//!
//! The file is untrusted input that ends up in the system prompt, so the
//! loader only accepts a regular file at the workspace root, refuses symlinks
//! (they could point outside the workspace) and enforces a size limit.

use std::path::{Path, PathBuf};

use thiserror::Error;

pub const FILE_NAME: &str = "AGENTS.md";
pub const MAX_BYTES: u64 = 32 * 1024;

#[derive(Debug, Error)]
pub enum AgentsError {
    #[error("cannot read {path}: {source}")]
    Read {
        path: PathBuf,
        source: std::io::Error,
    },
    #[error("{path} is a symlink; workspace instructions must be a regular file")]
    Symlink { path: PathBuf },
    #[error("{path} is not a regular file")]
    NotAFile { path: PathBuf },
    #[error("{path} is {bytes} bytes; the limit is {max} bytes", max = MAX_BYTES)]
    TooLarge { path: PathBuf, bytes: u64 },
}

/// Load `AGENTS.md` from the workspace root. Missing or blank files are `None`.
pub fn load(workspace: &Path) -> Result<Option<String>, AgentsError> {
    let path = workspace.join(FILE_NAME);
    let metadata = match std::fs::symlink_metadata(&path) {
        Ok(metadata) => metadata,
        Err(err) if err.kind() == std::io::ErrorKind::NotFound => return Ok(None),
        Err(source) => return Err(AgentsError::Read { path, source }),
    };

    if metadata.file_type().is_symlink() {
        return Err(AgentsError::Symlink { path });
    }
    if !metadata.is_file() {
        return Err(AgentsError::NotAFile { path });
    }
    if metadata.len() > MAX_BYTES {
        return Err(AgentsError::TooLarge {
            path,
            bytes: metadata.len(),
        });
    }

    let text = std::fs::read_to_string(&path).map_err(|source| AgentsError::Read {
        path: path.clone(),
        source,
    })?;
    let trimmed = text.trim();
    if trimmed.is_empty() {
        Ok(None)
    } else {
        Ok(Some(trimmed.to_string()))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn missing_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn loads_and_trims_the_file() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "\n  use tabs; no TODOs  \n").unwrap();
        assert_eq!(
            load(dir.path()).unwrap().as_deref(),
            Some("use tabs; no TODOs")
        );
    }

    #[test]
    fn blank_file_is_none() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(dir.path().join(FILE_NAME), "  \n\n").unwrap();
        assert_eq!(load(dir.path()).unwrap(), None);
    }

    #[test]
    fn oversized_file_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::write(
            dir.path().join(FILE_NAME),
            "x".repeat(MAX_BYTES as usize + 1),
        )
        .unwrap();
        assert!(matches!(
            load(dir.path()),
            Err(AgentsError::TooLarge { .. })
        ));
    }

    #[cfg(unix)]
    #[test]
    fn symlink_is_rejected() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("outside.md"), "secret").unwrap();
        std::os::unix::fs::symlink(
            outside.path().join("outside.md"),
            dir.path().join(FILE_NAME),
        )
        .unwrap();
        assert!(matches!(load(dir.path()), Err(AgentsError::Symlink { .. })));
    }
}
