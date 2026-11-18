//! Watch the config file for changes this process will not pick up (ADR-0103).
//!
//! `assemble` builds the egress proxy and the OS sandbox in one pass, and those
//! two decide what the agent may touch. `qc gui` re-assembles them when the file
//! changes; `qc tui` and `qcd` do not, so a tightened `[sandbox]` silently keeps
//! running with the old grants until the process restarts.
//!
//! ADR-0103 requires those two frontends to *say* so rather than reload. This is
//! that watcher: it holds the mtime seen at startup, compares on demand, and
//! reports the change exactly once so a caller polling in a loop does not repeat
//! the same warning every tick.

use std::path::{Path, PathBuf};
use std::time::SystemTime;

/// The path and mtime of the config file a process was assembled from.
///
/// `path` is `None` when the file could not be located at all: there is nothing
/// to watch, and saying "the config changed" about a file that never existed
/// would be a lie.
#[derive(Debug, Clone)]
pub struct ConfigWatch {
    path: Option<PathBuf>,
    seen: Option<SystemTime>,
    /// Set once `changed` has reported it, so the warning is emitted once.
    reported: bool,
}

impl ConfigWatch {
    /// Watch `path`, recording the mtime it has right now.
    pub fn new(path: Option<PathBuf>) -> Self {
        let seen = path.as_deref().and_then(mtime_of);
        Self {
            path,
            seen,
            reported: false,
        }
    }

    /// The file being watched, if one was found.
    pub fn path(&self) -> Option<&Path> {
        self.path.as_deref()
    }

    /// Has the file changed since `new`? Returns the path once, then `None`.
    ///
    /// A missing file after startup counts as a change: the config was removed
    /// or the directory stopped being readable, and the running process is still
    /// using what it read at startup either way.
    pub fn changed(&mut self) -> Option<&Path> {
        if self.reported {
            return None;
        }
        let path = self.path.as_deref()?;
        let now = mtime_of(path);
        if now == self.seen {
            return None;
        }
        self.seen = now;
        self.reported = true;
        Some(path)
    }

    /// The warning to show when [`Self::changed`] fires.
    ///
    /// One sentence, but it has to carry three facts: the file changed, this
    /// process is still on the old config, and only a restart fixes that. The
    /// first draft said just "config changed", which reads like a no-op.
    pub fn warning(path: &Path) -> String {
        format!(
            "{} changed on disk; this process is still running the config it loaded at startup \
             (sandbox and network settings are unchanged) — restart to apply",
            path.display()
        )
    }
}

fn mtime_of(path: &Path) -> Option<SystemTime> {
    std::fs::metadata(path)
        .and_then(|meta| meta.modified())
        .ok()
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A file that exists but is not touched must not report a change: a watcher
    /// that fires on startup would train people to ignore the warning.
    #[test]
    fn an_untouched_file_does_not_report_a_change() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\n").expect("write");

        let mut watch = ConfigWatch::new(Some(path));
        assert!(watch.changed().is_none());
        assert!(watch.changed().is_none());
    }

    /// A touched file reports exactly once. Polling every frame would otherwise
    /// repeat the same line forever, which buries it instead of surfacing it.
    #[test]
    fn a_touched_file_reports_once() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\n").expect("write");

        let mut watch = ConfigWatch::new(Some(path.clone()));
        touch_later(&path);
        assert_eq!(watch.changed(), Some(path.as_path()));
        assert!(watch.changed().is_none(), "must not repeat");
    }

    /// No path means nothing to watch — and no claim that something changed.
    #[test]
    fn no_path_never_reports() {
        let mut watch = ConfigWatch::new(None);
        assert!(watch.changed().is_none());
    }

    /// Deleting the file after startup is a change: the process still holds the
    /// grants it read, and the user needs to know the file no longer matches.
    #[test]
    fn a_deleted_file_counts_as_a_change() {
        let dir = tempdir();
        let path = dir.join("config.toml");
        std::fs::write(&path, "version = 1\n").expect("write");

        let mut watch = ConfigWatch::new(Some(path.clone()));
        std::fs::remove_file(&path).expect("remove");
        assert_eq!(watch.changed(), Some(path.as_path()));
    }

    /// The warning must name the file and say that a restart is needed; a
    /// generic "config changed" reads like something already handled.
    #[test]
    fn the_warning_says_what_to_do() {
        let text = ConfigWatch::warning(Path::new("/home/q/.config/qcode/config.toml"));
        assert!(text.contains("/home/q/.config/qcode/config.toml"), "{text}");
        assert!(text.contains("restart"), "{text}");
        assert!(text.contains("still running"), "{text}");
    }

    fn tempdir() -> PathBuf {
        let path = std::env::temp_dir().join(format!(
            "qc-configwatch-{}-{}",
            std::process::id(),
            std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_nanos())
                .unwrap_or(0)
        ));
        std::fs::create_dir_all(&path).expect("create temp dir");
        path
    }

    /// Bump the mtime without depending on filesystem timestamp granularity:
    /// some filesystems only record whole seconds, so writing the same content
    /// twice in the same second would not register.
    fn touch_later(path: &Path) {
        let later = SystemTime::now() + std::time::Duration::from_secs(2);
        let file = std::fs::File::options()
            .write(true)
            .open(path)
            .expect("open for touch");
        file.set_modified(later).expect("set mtime");
    }
}
