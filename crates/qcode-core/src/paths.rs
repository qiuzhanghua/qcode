//! Roots that must never be handed to a subprocess as **writable**.
//!
//! A policy predicate, not a layout: it names no distribution path, no drive
//! letter and no OS convention, only the two shapes that are always a mistake —
//! the filesystem root, and the directory that holds every user's home
//! (including this one).
//!
//! Lives here because both sides of the rule need it and neither may import the
//! other: `qcode-mcp` derives per-server write grants (ADR-0066) and must stay
//! free of the tools crate, while `qcode-tools`/`qcode-kernel` enforce
//! `[sandbox] extra_writable`. Two copies of this list would drift into "the
//! server grant refuses `/` but the sandbox grants it", and no test would catch
//! the gap because each copy is self-consistent.
//!
//! The reason is spelled out rather than a bare `bool`: it ends up in an error
//! the user has to act on, and "`~/` is your home directory" is the sentence
//! that explains which two characters went wrong.

use std::path::{Path, PathBuf};

/// The current user's home directory, or `None` when the environment does not
/// say.
///
/// One resolver for the whole workspace. `qcode-kernel` had its own and
/// `qcode-mcp` had another, which is two chances to disagree about what `~/`
/// means — and `~/` now appears in three different config sections
/// (ADR-0066 §2 修订注记). `qcode-mcp::real_home` delegates here.
pub fn real_home() -> Option<PathBuf> {
    #[cfg(windows)]
    {
        std::env::var_os("USERPROFILE")
            .or_else(|| std::env::var_os("HOME"))
            .map(PathBuf::from)
    }
    #[cfg(not(windows))]
    {
        std::env::var_os("HOME").map(PathBuf::from)
    }
}

/// Why granting **write** access to `path` is refused, if it is.
///
/// `real_home` is the current user's home, or `None` when it cannot be
/// determined — in which case the home check is skipped rather than guessed.
pub fn dangerous_write_root(path: &Path, real_home: Option<&Path>) -> Option<&'static str> {
    if path == Path::new("/") {
        return Some("the filesystem root");
    }
    if let Some(home) = real_home
        && path == home
    {
        return Some("your home directory");
    }
    for name in ["/home", "/Users", "/root"] {
        if path == Path::new(name) {
            return Some("a directory that contains every user's home");
        }
    }
    None
}

/// The one place that decides what lives under the data directory's `logs/`
/// (ADR-0077).
///
/// Three callers used to spell out `data_dir.join("logs")` themselves — the
/// process log, the MCP server stderr files and the network audit trail — and
/// they sit in three crates that cannot all import each other (`qcode-kernel`
/// depends on `qcode-mcp`, so the reverse dependency would be a cycle). Sharing
/// the *name* through the crate they do have in common is cheaper than a
/// constant each copy redefines.
///
/// A layout function, unlike the predicates above: it names a directory. It goes
/// here because `qcode-core` is the only crate all three callers depend on.
pub fn log_dir(data_dir: &Path) -> PathBuf {
    data_dir.join("logs")
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::path::PathBuf;

    #[test]
    fn the_root_is_never_writable() {
        assert_eq!(
            dangerous_write_root(Path::new("/"), None),
            Some("the filesystem root")
        );
    }

    #[test]
    fn the_home_directory_is_never_writable() {
        let home = PathBuf::from("/home/q");
        assert_eq!(
            dangerous_write_root(&home, Some(&home)),
            Some("your home directory")
        );
        // A subdirectory of the home is the whole point of the check: that is
        // the shape `~/Documents` produces, and it must pass.
        assert_eq!(
            dangerous_write_root(&home.join("Documents"), Some(&home)),
            None
        );
    }

    #[test]
    fn a_home_parent_is_never_writable() {
        for name in ["/home", "/Users", "/root"] {
            assert!(
                dangerous_write_root(Path::new(name), None).is_some(),
                "{name} slipped through"
            );
        }
    }

    /// Without a home to compare against the check degrades to the three fixed
    /// shapes rather than guessing one.
    #[test]
    fn no_known_home_skips_only_the_home_comparison() {
        assert_eq!(dangerous_write_root(Path::new("/srv/data"), None), None);
        assert!(dangerous_write_root(Path::new("/"), None).is_some());
    }
}
