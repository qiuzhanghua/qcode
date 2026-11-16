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

/// The workspace a frontend starts in when the user did not name one.
///
/// **One rule for every entry point** (ADR-0090 §2): the current directory,
/// except that a filesystem root is not a usable answer — it falls back to the
/// home directory. `/` is what `launchd` hands a Dock-launched GUI (ADR-0085),
/// and it is also what `cd / && qc ...` would otherwise hand a scheduled task.
///
/// The second return value says whether the fallback happened. Every caller
/// needs to know: the GUI logs it, the CLI prints it. A silent substitution of
/// the directory the user was standing in is the failure this rule exists to
/// prevent.
///
/// Lives here, not in the binary, because three call sites need the *same*
/// answer (ADR-0090 §0.3): the GUI's `gui_workspace`, `qc schedule create`'s
/// default, and `schedule_create`'s fallback for a delivery turn. Copies would
/// drift, and the drift would be silent — each copy is self-consistent.
pub fn choose_workspace(cwd: PathBuf, home: Option<PathBuf>) -> (PathBuf, bool) {
    // A root has no parent: `/` on Unix, `C:\` on Windows. One rule for both,
    // rather than a per-platform list of "bad" directories to keep in sync
    // (and deliberately not a guess at what Windows shortcut defaults are).
    if cwd.parent().is_some() {
        return (cwd, false);
    }
    match home {
        Some(home) => (home, true),
        // No home to fall back to: the cwd we have beats failing to start at
        // all.
        None => (cwd, false),
    }
}

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

/// The log file this process is writing to.
///
/// `logs/qc.log` is a symlink to the current daily file (ADR-0082). A `qc log`
/// invocation reads it, and so does the orphan scan in
/// [`crate::lifecycle`]; both need to resolve to the *same* file the writer
/// holds, which is why this is one function rather than two similar ones.
///
/// Falls back to the expected path when neither the link nor any file exists:
/// callers use it as "where the log would be", and a missing file is reported
/// by the reader, not by path resolution.
pub fn current_log(data_dir: &Path) -> PathBuf {
    let link = log_dir(data_dir).join(LOG_NAME);
    if link.exists() {
        return link;
    }
    latest_log_file(data_dir).unwrap_or(link)
}

/// Name of the stable entry point into the rolled logs.
pub const LOG_NAME: &str = "qc.log";

/// The most recent rolled file in `logs/`, if any.
///
/// Filtered and ordered **exactly** as `qc logs` orders them (`log_files`):
/// name starts with `qc.`, ends with `.log`, newest mtime last. Keeping the two
/// in step matters because `current_log` reads through this — a different order
/// here would make the orphan scan read a different file than the one the user
/// is looking at.
fn latest_log_file(data_dir: &Path) -> Option<PathBuf> {
    let dir = log_dir(data_dir);
    let entries = std::fs::read_dir(&dir).ok()?;
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            let name = entry.file_name();
            let name = name.to_string_lossy();
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && name.starts_with("qc.")
                && name.ends_with(".log")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    files.sort();
    files.pop().map(|(_, path)| path)
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

    #[test]
    fn a_real_directory_is_kept_as_the_workspace() {
        let cwd = PathBuf::from("/Users/someone/project");
        let (workspace, fell_back) =
            choose_workspace(cwd.clone(), Some(PathBuf::from("/Users/someone")));
        assert_eq!(workspace, cwd, "终端启动的 cwd 是用户的选择，不许改写");
        assert!(!fell_back);
    }

    #[test]
    fn the_filesystem_root_falls_back_to_home() {
        // 实测：Finder / Dock 启动给的就是这个（`open -a` 打出来的 cwd 是 `/`）。
        let (workspace, fell_back) =
            choose_workspace(PathBuf::from("/"), Some(PathBuf::from("/Users/someone")));
        assert_eq!(workspace, PathBuf::from("/Users/someone"));
        assert!(fell_back, "应当报告回退过，好让日志 / CLI 里留下一条");
    }

    #[test]
    fn a_root_without_a_home_is_still_usable() {
        // 没有家目录可退时，宁可拿 `/` 干活，也不要因为「路径不理想」拒绝启动。
        let (workspace, fell_back) = choose_workspace(PathBuf::from("/"), None);
        assert_eq!(workspace, PathBuf::from("/"));
        assert!(!fell_back);
    }
}
