//! Workspace-confined path resolution.
//!
//! Lexically normalizes `..`, then canonicalizes the deepest existing ancestor
//! so symlinks cannot escape the workspace either.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use qcode_core::tool::ToolError;

pub fn resolve(workspace: &Path, requested: &str) -> Result<PathBuf, ToolError> {
    if requested.trim().is_empty() {
        return Err(ToolError::Invalid("path is empty".to_string()));
    }

    let workspace = workspace
        .canonicalize()
        .map_err(|_| ToolError::Invalid(format!("workspace not found: {}", workspace.display())))?;

    let raw = Path::new(requested);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        workspace.join(raw)
    };
    let normalized = normalize(&absolute);

    let mut existing = normalized.clone();
    let mut suffix: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(OsString::from) else {
            return Err(ToolError::Invalid(format!(
                "cannot resolve path: {requested}"
            )));
        };
        suffix.push(name);
        existing = existing
            .parent()
            .map(Path::to_path_buf)
            .ok_or_else(|| ToolError::Invalid(format!("cannot resolve path: {requested}")))?;
    }

    let canonical = existing.canonicalize()?;
    if !canonical.starts_with(&workspace) {
        return Err(ToolError::PathEscape(requested.to_string()));
    }

    let mut resolved = canonical;
    for name in suffix.iter().rev() {
        resolved.push(name);
    }
    Ok(resolved)
}

/// Lexical normalization: resolves `.` and `..` without touching the filesystem.
fn normalize(path: &Path) -> PathBuf {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::ParentDir => {
                out.pop();
            }
            Component::CurDir => {}
            other => out.push(other.as_os_str()),
        }
    }
    out
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn normalizes_dot_segments() {
        let normalized = normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(normalized, PathBuf::from("/a/c/d"));
    }
}
