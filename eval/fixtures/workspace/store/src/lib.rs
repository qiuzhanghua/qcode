//! One-file-per-key key/value store with atomic writes.
//!
//! Keys are restricted to ASCII letters, digits, `-` and `_`; values are
//! whitespace-normalized before they are written.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

pub struct Store {
    root: PathBuf,
}

impl Store {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self { root: root.into() }
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    pub fn get(&self, key: &str) -> io::Result<Option<String>> {
        let path = self.key_path(key)?;
        match fs::read_to_string(&path) {
            Ok(value) => Ok(Some(value)),
            Err(err) if err.kind() == io::ErrorKind::NotFound => Ok(None),
            Err(err) => Err(err),
        }
    }

    pub fn set(&self, key: &str, value: &str) -> io::Result<()> {
        let path = self.key_path(key)?;
        fs::create_dir_all(&self.root)?;

        // Write a sibling file first so a crash never leaves a half-written
        // value behind.
        let temp = self.root.join(format!(".{key}.tmp"));
        fs::write(&temp, normalize(value))?;
        fs::rename(&temp, &path)
    }

    pub fn list(&self) -> io::Result<Vec<String>> {
        let mut keys = Vec::new();
        let entries = match fs::read_dir(&self.root) {
            Ok(entries) => entries,
            Err(err) if err.kind() == io::ErrorKind::NotFound => return Ok(keys),
            Err(err) => return Err(err),
        };

        for entry in entries {
            let entry = entry?;
            let name = entry.file_name();
            let name = name.to_string_lossy();
            if let Some(key) = name.strip_suffix(".txt") {
                keys.push(key.to_string());
            }
        }
        keys.sort();
        Ok(keys)
    }

    fn key_path(&self, key: &str) -> io::Result<PathBuf> {
        if key.is_empty() || key.len() > 64 {
            return Err(invalid("key must be 1..=64 bytes"));
        }
        if key.starts_with('.') || key.contains("..") {
            return Err(invalid("key must not start with `.` or contain `..`"));
        }
        if !key
            .chars()
            .all(|character| character.is_ascii_alphanumeric() || matches!(character, '-' | '_'))
        {
            return Err(invalid(
                "key may only contain ASCII letters, digits, `-` and `_`",
            ));
        }
        Ok(self.root.join(format!("{key}.txt")))
    }
}

fn invalid(message: &str) -> io::Error {
    io::Error::new(io::ErrorKind::InvalidInput, message)
}

fn normalize(value: &str) -> String {
    // Intentionally duplicated with fixture-core: task 03 unifies the copies.
    value.split_whitespace().collect::<Vec<_>>().join(" ")
}
