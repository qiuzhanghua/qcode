//! Workspace-confined path resolution.
//!
//! Lexically normalizes `..`, then canonicalizes the deepest existing ancestor
//! so symlinks cannot escape the workspace either.

use std::ffi::OsString;
use std::path::{Component, Path, PathBuf};

use qcode_core::task::{Root, RootAccess};
use qcode_core::tool::{ToolContext, ToolError};

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

#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Access {
    Read,
    Write,
}

#[derive(Debug)]
pub struct Resolved<'a> {
    pub path: PathBuf,
    pub root: &'a Root,
    /// `@label/path` form for tool output.
    pub display: String,
}

/// Resolve a path against a task's roots.
///
/// - `@label/path` selects a root explicitly.
/// - Relative paths use the primary root (or the first root if none is marked).
/// - Absolute paths must fall inside one root.
pub fn resolve_ctx<'a>(
    ctx: &'a ToolContext,
    requested: &str,
    access: Access,
) -> Result<Resolved<'a>, ToolError> {
    if requested.trim().is_empty() {
        return Err(ToolError::Invalid("path is empty".to_string()));
    }
    if ctx.roots.is_empty() {
        return Err(ToolError::NoRoots);
    }

    if let Some((label, rest)) = split_alias(requested) {
        let root = ctx
            .roots
            .iter()
            .find(|root| root.label == label)
            .ok_or_else(|| ToolError::Invalid(format!("unknown root '@{label}'")))?;
        let raw = if rest.is_empty() {
            PathBuf::from(".")
        } else {
            PathBuf::from(rest)
        };
        return resolve_in_root(root, &raw, access);
    }

    let raw = Path::new(requested);
    if raw.is_absolute() {
        let normalized = strip_verbatim(&normalize(raw));
        let root = ctx
            .roots
            .iter()
            .filter_map(|root| {
                root.path
                    .canonicalize()
                    .ok()
                    .map(|canonical| (root, strip_verbatim(&canonical)))
            })
            .filter(|(_, canonical)| normalized.starts_with(canonical))
            .max_by_key(|(_, canonical)| canonical.as_os_str().len())
            .map(|(root, _)| root)
            .ok_or_else(|| ToolError::PathEscape(requested.to_string()))?;
        return resolve_in_root(root, raw, access);
    }

    let root = ctx
        .roots
        .iter()
        .find(|root| root.primary)
        .or_else(|| ctx.roots.first())
        .expect("roots checked above");
    resolve_in_root(root, raw, access)
}

fn split_alias(requested: &str) -> Option<(&str, &str)> {
    let rest = requested.strip_prefix('@')?;
    match rest.split_once('/') {
        Some((label, path)) => Some((label, path)),
        None => Some((rest, "")),
    }
}

/// Windows `canonicalize` returns `\\?\`-prefixed (verbatim) paths; the
/// model's absolute paths and the folder picker do not. Strip the prefix so
/// prefix-agnostic `starts_with` comparisons still match. On other platforms
/// this is the identity.
fn strip_verbatim(path: &Path) -> PathBuf {
    #[cfg(windows)]
    {
        let text = path.to_string_lossy();
        let stripped = text
            .strip_prefix(r"\\?\UNC\")
            .map(|rest| format!(r"\\{rest}"))
            .or_else(|| text.strip_prefix(r"\\?\").map(str::to_string));
        if let Some(stripped) = stripped {
            return PathBuf::from(stripped);
        }
    }
    path.to_path_buf()
}

fn resolve_in_root<'a>(
    root: &'a Root,
    raw: &Path,
    access: Access,
) -> Result<Resolved<'a>, ToolError> {
    let canonical_root = strip_verbatim(&root.path.canonicalize().map_err(|_| {
        // A vanished directory is a task- level problem, not a bad path: say
        // which root and what to do about it.
        ToolError::Invalid(format!(
            "root '@{}' is unavailable ({} is gone); relink the root before using it",
            root.label,
            root.path.display()
        ))
    })?);
    let absolute = if raw.is_absolute() {
        raw.to_path_buf()
    } else {
        root.path.join(raw)
    };
    let normalized = normalize(&absolute);
    let resolved = canonicalize_existing(&normalized, raw)?;
    if !resolved.starts_with(&canonical_root) {
        return Err(ToolError::PathEscape(raw.display().to_string()));
    }
    if access == Access::Write && root.access == RootAccess::ReadOnly {
        return Err(ToolError::ReadOnly(root.label.clone()));
    }
    let relative = resolved
        .strip_prefix(&canonical_root)
        .unwrap_or(Path::new(""));
    let display = if relative.as_os_str().is_empty() {
        format!("@{}", root.label)
    } else {
        format!("@{}/{}", root.label, relative.display())
    };
    Ok(Resolved {
        path: resolved,
        root,
        display,
    })
}

fn canonicalize_existing(path: &Path, requested: &Path) -> Result<PathBuf, ToolError> {
    let mut existing = path.to_path_buf();
    let mut suffix: Vec<OsString> = Vec::new();
    while !existing.exists() {
        let Some(name) = existing.file_name().map(OsString::from) else {
            return Err(ToolError::Invalid(format!(
                "cannot resolve path: {}",
                requested.display()
            )));
        };
        suffix.push(name);
        existing = existing.parent().map(Path::to_path_buf).ok_or_else(|| {
            ToolError::Invalid(format!("cannot resolve path: {}", requested.display()))
        })?;
    }
    let mut resolved = strip_verbatim(&existing.canonicalize()?);
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
    fn resolves_alias_read_and_write_inside_root() {
        let dir = tempfile::tempdir().unwrap();
        let file = dir.path().join("hello.txt");
        std::fs::write(&file, "hi").unwrap();
        let ctx = ToolContext::for_test(dir.path());
        let read = resolve_ctx(&ctx, "@workspace/hello.txt", Access::Read).unwrap();
        assert_eq!(read.path, strip_verbatim(&file.canonicalize().unwrap()));
        assert_eq!(read.display, "@workspace/hello.txt");

        let write = resolve_ctx(&ctx, "new.txt", Access::Write).unwrap();
        assert_eq!(
            write.path,
            strip_verbatim(&dir.path().canonicalize().unwrap()).join("new.txt")
        );
    }

    #[test]
    fn rejects_escape_unknown_alias_and_readonly_write() {
        let dir = tempfile::tempdir().unwrap();
        let outside = tempfile::tempdir().unwrap();
        std::fs::write(outside.path().join("secret"), "x").unwrap();
        let ctx = ToolContext::for_test(dir.path());
        assert!(resolve_ctx(&ctx, "../secret", Access::Read).is_err());
        assert!(resolve_ctx(&ctx, "@nope/file", Access::Read).is_err());
        assert!(
            resolve_ctx(
                &ctx,
                outside.path().join("secret").to_str().unwrap(),
                Access::Read
            )
            .is_err()
        );

        let root = Root {
            id: "docs".into(),
            label: "docs".into(),
            path: outside.path().to_path_buf(),
            access: RootAccess::ReadOnly,
            primary: false,
        };
        let ctx = ctx.with_roots(vec![root]);
        assert!(matches!(
            resolve_ctx(&ctx, "@docs/secret", Access::Write),
            Err(ToolError::ReadOnly(_))
        ));
        assert!(resolve_ctx(&ctx, "@docs/secret", Access::Read).is_ok());
    }

    #[test]
    fn no_roots_is_an_error() {
        let ctx = ToolContext::for_test("/tmp").with_roots(Vec::new());
        assert!(matches!(
            resolve_ctx(&ctx, "x", Access::Read),
            Err(ToolError::NoRoots)
        ));
    }

    #[test]
    fn normalizes_dot_segments() {
        let normalized = normalize(Path::new("/a/b/../c/./d"));
        assert_eq!(normalized, PathBuf::from("/a/c/d"));
    }
}
