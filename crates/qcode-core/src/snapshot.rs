//! Pre-images of files the agent is about to modify.
//!
//! A checkpoint is not a copy of the workspace: it is the set of files that were
//! *about to change*, captured before the change. Rolling back to turn `N`
//! restores the oldest pre-image recorded at or after `N` for every path, and
//! deletes the files that did not exist at that boundary. Files the agent never
//! touched are left alone.
//!
//! Layout under `<data-dir>/snapshots/<session>/<turn>/`:
//!
//! - `files/<workspace-relative path>` — the content before the write;
//! - `absent.txt` — paths that did not exist yet (one per line), so a rollback
//!   can remove them instead of leaving an empty file behind.

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;

/// Which paths a tool call is going to modify.
///
/// This is a *hint*, used only to decide what to snapshot. The tools stay
/// authoritative for what they actually write, so a missed hint costs a
/// rollback, never correctness of the write itself.
pub fn write_hints(tool: &str, args: &Value) -> Vec<PathBuf> {
    match tool {
        "write_file" | "edit_file" => args
            .get("path")
            .and_then(Value::as_str)
            .filter(|path| !path.trim().is_empty())
            .map(|path| vec![PathBuf::from(path)])
            .unwrap_or_default(),
        "apply_patch" => args
            .get("patch")
            .and_then(Value::as_str)
            .map(patch_paths)
            .unwrap_or_default(),
        _ => Vec::new(),
    }
}

/// Paths named by a unified diff in the `apply_patch` format.
fn patch_paths(patch: &str) -> Vec<PathBuf> {
    let mut paths = Vec::new();
    for line in patch.lines() {
        let trimmed = line.trim_end();
        for prefix in [
            "*** Update File: ",
            "*** Add File: ",
            "*** Delete File: ",
            "*** Move to: ",
        ] {
            if let Some(rest) = trimmed.strip_prefix(prefix) {
                let path = rest.trim();
                if !path.is_empty()
                    && !paths
                        .iter()
                        .any(|known: &PathBuf| known.as_path() == Path::new(path))
                {
                    paths.push(PathBuf::from(path));
                }
            }
        }
    }
    paths
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct RestoreReport {
    /// Turns whose pre-images were considered, oldest first.
    pub turns: Vec<u32>,
    pub restored: usize,
    pub deleted: usize,
    /// Files recorded as absent that were absent already.
    pub already_absent: usize,
    /// Paths that pointed outside the workspace and were refused.
    pub refused: Vec<String>,
}

#[derive(Debug, Clone)]
pub struct SnapshotStore {
    root: PathBuf,
    /// Also snapshot the whole workspace before `run_shell`, which can write
    /// anywhere. Off by default in tests, on by default from the config.
    shell: bool,
}

/// Limits for a workspace-wide snapshot taken before a shell command.
const TREE_MAX_FILE: u64 = 4 * 1024 * 1024;
const TREE_MAX_TOTAL: u64 = 64 * 1024 * 1024;
/// Directories a shell command rarely means to edit, and that are large.
const TREE_SKIP: &[&str] = &[
    ".git",
    ".hg",
    ".svn",
    "target",
    "node_modules",
    "dist",
    ".tmp",
    ".qcode-data",
    ".venv",
    "__pycache__",
];

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeReport {
    pub files: usize,
    pub bytes: u64,
    pub skipped_large: usize,
    /// The total cap was hit; the snapshot is partial.
    pub capped: bool,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            shell: false,
        }
    }

    /// `<data-dir>/snapshots`, the same place the session log lives under.
    pub fn at_data_dir(data_dir: &Path) -> Self {
        Self::new(data_dir.join("snapshots"))
    }

    /// Snapshot the workspace before shell commands (`[agent] snapshot_shell`).
    pub fn with_shell(mut self, shell: bool) -> Self {
        self.shell = shell;
        self
    }

    pub fn shell_enabled(&self) -> bool {
        self.shell
    }

    pub fn root(&self) -> &Path {
        &self.root
    }

    fn session_dir(&self, session: &str) -> PathBuf {
        self.root.join(sanitize(session))
    }

    /// Record the current content of `paths` as the pre-image for `turn`.
    /// Paths that do not exist are recorded as absent. Existing records for the
    /// same turn and path are left untouched: the first write in a turn wins,
    /// because that is the state a rollback has to restore.
    pub fn record(
        &self,
        session: &str,
        turn: u32,
        primary_root: &Path,
        paths: &[PathBuf],
    ) -> io::Result<usize> {
        let dir = self.session_dir(session).join(turn.to_string());
        let mut recorded = 0;
        for path in paths {
            let Some(relative) = relative_to(primary_root, path) else {
                continue;
            };
            let stored = dir.join("files").join(&relative);
            let mut absent = read_absent(&dir)?;
            if stored.exists() || absent.iter().any(|known| known == &relative) {
                continue;
            }
            match std::fs::read(primary_root.join(&relative)) {
                Ok(bytes) => {
                    if let Some(parent) = stored.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&stored, bytes)?;
                    recorded += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    absent.push(relative);
                    write_absent(&dir, &absent)?;
                    recorded += 1;
                }
                Err(error) => return Err(error),
            }
        }
        Ok(recorded)
    }

    /// Copy the whole workspace as the pre-image for `turn`, bounded by
    /// [`TREE_MAX_FILE`] / [`TREE_MAX_TOTAL`] and skipping build/VCS trees.
    ///
    /// Used before `run_shell`, which can write any file: the alternative is
    /// guessing paths from a command line, and a wrong guess loses the file.
    pub fn snapshot_tree(
        &self,
        session: &str,
        turn: u32,
        primary_root: &Path,
    ) -> io::Result<TreeReport> {
        let dir = self
            .session_dir(session)
            .join(turn.to_string())
            .join("files");
        let mut report = TreeReport::default();
        let mut stack = vec![primary_root.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            for entry in entries.flatten() {
                let path = entry.path();
                let name = entry.file_name();
                let name = name.to_string_lossy();
                if path.is_dir() {
                    if !TREE_SKIP.contains(&name.as_ref()) {
                        stack.push(path);
                    }
                    continue;
                }
                let Ok(relative) = path.strip_prefix(primary_root) else {
                    continue;
                };
                let Ok(metadata) = entry.metadata() else {
                    continue;
                };
                if !metadata.is_file() {
                    continue;
                }
                if metadata.len() > TREE_MAX_FILE {
                    report.skipped_large += 1;
                    continue;
                }
                if report.bytes + metadata.len() > TREE_MAX_TOTAL {
                    report.capped = true;
                    break;
                }
                let stored = dir.join(relative);
                if stored.exists() {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                if let Some(parent) = stored.parent() {
                    std::fs::create_dir_all(parent)?;
                }
                std::fs::write(&stored, &bytes)?;
                report.files += 1;
                report.bytes += bytes.len() as u64;
            }
            if report.capped {
                break;
            }
        }
        Ok(report)
    }

    /// Drop everything recorded for one session (used by `sessions gc`).
    pub fn remove_session(&self, session: &str) -> io::Result<bool> {
        let dir = self.session_dir(session);
        if !dir.exists() {
            return Ok(false);
        }
        std::fs::remove_dir_all(&dir)?;
        Ok(true)
    }

    /// Remove snapshot directories whose session no longer exists. Returns the
    /// names that were removed.
    pub fn sweep(&self, keep: &[String]) -> io::Result<Vec<String>> {
        let mut removed = Vec::new();
        let Ok(entries) = std::fs::read_dir(&self.root) else {
            return Ok(removed);
        };
        for entry in entries.flatten() {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            let name = entry.file_name().to_string_lossy().into_owned();
            if keep.iter().any(|session| sanitize(session) == name) {
                continue;
            }
            std::fs::remove_dir_all(entry.path())?;
            removed.push(name);
        }
        removed.sort();
        Ok(removed)
    }

    /// Bytes currently stored for one session, for the gc report.
    pub fn size(&self, session: &str) -> io::Result<u64> {
        fn walk(dir: &Path, total: &mut u64) -> io::Result<()> {
            let Ok(entries) = std::fs::read_dir(dir) else {
                return Ok(());
            };
            for entry in entries.flatten() {
                let metadata = entry.metadata()?;
                if metadata.is_dir() {
                    walk(&entry.path(), total)?;
                } else {
                    *total += metadata.len();
                }
            }
            Ok(())
        }
        let mut total = 0;
        walk(&self.session_dir(session), &mut total)?;
        Ok(total)
    }

    /// Turns that have at least one recorded pre-image, oldest first.
    pub fn turns(&self, session: &str) -> io::Result<Vec<u32>> {
        let dir = self.session_dir(session);
        let mut turns = Vec::new();
        let Ok(entries) = std::fs::read_dir(&dir) else {
            return Ok(turns);
        };
        for entry in entries.flatten() {
            if !entry.file_type()?.is_dir() {
                continue;
            }
            if let Ok(turn) = entry.file_name().to_string_lossy().parse::<u32>() {
                turns.push(turn);
            }
        }
        turns.sort_unstable();
        Ok(turns)
    }

    /// Files recorded for one turn, as workspace-relative paths.
    pub fn files(&self, session: &str, turn: u32) -> io::Result<Vec<String>> {
        let dir = self.session_dir(session).join(turn.to_string());
        let mut files = Vec::new();
        let files_dir = dir.join("files");
        if files_dir.is_dir() {
            let mut stored = Vec::new();
            collect_files(&files_dir, &files_dir, &mut stored)?;
            files.extend(
                stored
                    .into_iter()
                    .map(|path| path.to_string_lossy().into_owned()),
            );
        }
        for absent in read_absent(&dir)? {
            files.push(format!("{} (absent)", absent.to_string_lossy()));
        }
        files.sort();
        Ok(files)
    }

    /// Restore the workspace to the state before `from_turn`.
    ///
    /// For every path recorded at a turn `>= from_turn`, the *earliest* record
    /// wins, because that is the content the file had when the checkpoint was
    /// taken. Newer records for the same path describe intermediate states.
    pub fn restore(
        &self,
        session: &str,
        from_turn: u32,
        primary_root: &Path,
    ) -> io::Result<RestoreReport> {
        let mut report = RestoreReport::default();
        let mut pre_images: BTreeMap<PathBuf, Option<PathBuf>> = BTreeMap::new();

        for turn in self.turns(session)? {
            if turn < from_turn {
                continue;
            }
            report.turns.push(turn);
            let dir = self.session_dir(session).join(turn.to_string());
            for absent in read_absent(&dir)? {
                pre_images.entry(absent).or_insert(None);
            }
            let mut stored = Vec::new();
            let files_dir = dir.join("files");
            if files_dir.is_dir() {
                collect_files(&files_dir, &files_dir, &mut stored)?;
            }
            for path in stored {
                pre_images
                    .entry(path)
                    .or_insert_with(|| Some(dir.join("files")));
            }
        }

        for (relative, source) in pre_images {
            let target = primary_root.join(&relative);
            match source {
                Some(stored_dir) => {
                    let from = stored_dir.join(&relative);
                    let bytes = std::fs::read(&from)?;
                    // A directory where a file is expected is a conflict we
                    // cannot resolve safely; report and skip instead of
                    // deleting the tree.
                    if target.is_dir() {
                        report.refused.push(format!(
                            "{}: a directory occupies this path",
                            relative.display()
                        ));
                        continue;
                    }
                    if let Some(parent) = target.parent() {
                        std::fs::create_dir_all(parent)?;
                    }
                    std::fs::write(&target, bytes)?;
                    report.restored += 1;
                }
                None => {
                    if target.exists() {
                        std::fs::remove_file(&target)?;
                        report.deleted += 1;
                    } else {
                        report.already_absent += 1;
                    }
                }
            }
        }
        Ok(report)
    }
}

/// Turn directory lookups must not escape the data dir; session ids are ULIDs
/// today but the log is user-editable input like any other.
fn sanitize(session: &str) -> String {
    session
        .chars()
        .map(|ch| {
            if ch.is_ascii_alphanumeric() || ch == '-' || ch == '_' {
                ch
            } else {
                '_'
            }
        })
        .collect()
}

/// `path` relative to `workspace`, refusing `..` and absolute forms that do not
/// start with the workspace.
fn relative_to(primary_root: &Path, path: &Path) -> Option<PathBuf> {
    let absolute = if path.is_absolute() {
        path.to_path_buf()
    } else {
        primary_root.join(path)
    };
    let normalized = normalize(&absolute)?;
    let root = normalize(primary_root)?;
    normalized.strip_prefix(&root).ok().map(Path::to_path_buf)
}

fn normalize(path: &Path) -> Option<PathBuf> {
    let mut out = PathBuf::new();
    for component in path.components() {
        match component {
            Component::CurDir => {}
            Component::ParentDir => {
                if !out.pop() {
                    return None;
                }
            }
            other => out.push(other.as_os_str()),
        }
    }
    Some(out)
}

fn read_absent(dir: &Path) -> io::Result<Vec<PathBuf>> {
    let path = dir.join("absent.txt");
    match std::fs::read_to_string(&path) {
        Ok(text) => Ok(text
            .lines()
            .filter(|line| !line.trim().is_empty())
            .map(PathBuf::from)
            .collect()),
        Err(error) if error.kind() == io::ErrorKind::NotFound => Ok(Vec::new()),
        Err(error) => Err(error),
    }
}

fn write_absent(dir: &Path, paths: &[PathBuf]) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let mut text = String::new();
    for path in paths {
        text.push_str(&path.to_string_lossy());
        text.push('\n');
    }
    std::fs::write(dir.join("absent.txt"), text)
}

fn collect_files(root: &Path, dir: &Path, out: &mut Vec<PathBuf>) -> io::Result<()> {
    for entry in std::fs::read_dir(dir)?.flatten() {
        let path = entry.path();
        if path.is_dir() {
            collect_files(root, &path, out)?;
        } else if let Ok(relative) = path.strip_prefix(root) {
            out.push(relative.to_path_buf());
        }
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn fixture() -> (tempfile::TempDir, SnapshotStore, PathBuf) {
        let dir = tempfile::tempdir().expect("temp dir");
        let workspace = dir.path().join("ws");
        std::fs::create_dir_all(&workspace).expect("workspace");
        let store = SnapshotStore::new(dir.path().join("snapshots"));
        (dir, store, workspace)
    }

    #[test]
    fn hints_name_the_paths_the_tools_write() {
        assert_eq!(
            write_hints(
                "write_file",
                &serde_json::json!({"path": "src/a.rs", "content": "x"})
            ),
            vec![PathBuf::from("src/a.rs")]
        );
        assert_eq!(
            write_hints("edit_file", &serde_json::json!({"path": "src/b.rs"})),
            vec![PathBuf::from("src/b.rs")]
        );
        assert!(write_hints("read_file", &serde_json::json!({"path": "src/a.rs"})).is_empty());
        assert!(write_hints("run_shell", &serde_json::json!({"command": "rm -rf /"})).is_empty());

        let patch =
            "*** Begin Patch\n*** Update File: src/a.rs\n*** Add File: src/new.rs\n*** End Patch\n";
        assert_eq!(
            write_hints("apply_patch", &serde_json::json!({"patch": patch})),
            vec![PathBuf::from("src/a.rs"), PathBuf::from("src/new.rs")]
        );
    }

    #[test]
    fn a_new_file_is_recorded_as_absent_and_removed_by_rollback() {
        let (_dir, store, workspace) = fixture();
        store
            .record("s1", 1, &workspace, &[PathBuf::from("created.txt")])
            .expect("record");

        let turns = store.turns("s1").expect("turns");
        assert_eq!(turns, vec![1]);
        assert_eq!(
            store.files("s1", 1).expect("files"),
            vec!["created.txt (absent)".to_string()]
        );

        std::fs::write(workspace.join("created.txt"), "hello").expect("write");
        let report = store.restore("s1", 1, &workspace).expect("restore");
        assert_eq!(report.deleted, 1);
        assert_eq!(report.restored, 0);
        assert!(!workspace.join("created.txt").exists());
    }

    #[test]
    fn rollback_restores_the_earliest_pre_image_at_or_after_the_turn() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "v1").expect("write");

        // Turn 1 changes it to v2, turn 2 to v3.
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("record t1");
        std::fs::write(workspace.join("a.txt"), "v2").expect("write");
        store
            .record("s1", 2, &workspace, &[PathBuf::from("a.txt")])
            .expect("record t2");
        std::fs::write(workspace.join("a.txt"), "v3").expect("write");

        // Rolling back to turn 2 restores the state before it: v2.
        let report = store.restore("s1", 2, &workspace).expect("restore");
        assert_eq!(report.turns, vec![2]);
        assert_eq!(report.restored, 1);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "v2"
        );

        // Rolling back to turn 1 restores v1.
        let report = store.restore("s1", 1, &workspace).expect("restore all");
        assert_eq!(report.turns, vec![1, 2]);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "v1"
        );
    }

    #[test]
    fn a_path_that_escapes_the_workspace_is_refused_not_recorded() {
        let (_dir, store, workspace) = fixture();
        let recorded = store
            .record("s1", 1, &workspace, &[PathBuf::from("../outside.txt")])
            .expect("record");
        assert_eq!(recorded, 0);
        assert!(store.turns("s1").expect("turns").is_empty());
    }

    #[test]
    fn the_first_write_in_a_turn_wins() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "before").expect("write");
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("first");
        std::fs::write(workspace.join("a.txt"), "during").expect("write");
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("second");

        let report = store.restore("s1", 1, &workspace).expect("restore");
        assert_eq!(report.restored, 1);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "before"
        );
    }

    #[test]
    fn a_missing_session_restores_nothing() {
        let (_dir, store, workspace) = fixture();
        let report = store.restore("nope", 1, &workspace).expect("restore");
        assert_eq!(report, RestoreReport::default());
    }

    #[test]
    fn a_workspace_snapshot_covers_files_a_shell_command_edits() {
        let (_dir, store, workspace) = fixture();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/a.rs"), "v1").unwrap();
        std::fs::write(workspace.join("b.txt"), "keep").unwrap();
        std::fs::create_dir_all(workspace.join("target")).unwrap();
        std::fs::write(workspace.join("target/junk"), "build output").unwrap();

        let report = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        // `target/` is on the skip list: two files, not three.
        assert_eq!(report.files, 2, "{report:?}");
        assert!(!report.capped);

        // What a shell command might do: edit one file, delete another, create a
        // third.
        std::fs::write(workspace.join("src/a.rs"), "v2").unwrap();
        std::fs::remove_file(workspace.join("b.txt")).unwrap();
        std::fs::write(workspace.join("new.txt"), "created by the shell").unwrap();

        let report = store.restore("s1", 1, &workspace).expect("restore");
        assert_eq!(report.restored, 2);
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/a.rs")).unwrap(),
            "v1"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("b.txt")).unwrap(),
            "keep"
        );
        // Created files are only removed when the tool that made them was one of
        // the writers that record `absent`; a tree snapshot cannot know.
        assert!(workspace.join("new.txt").exists());
    }

    #[test]
    fn a_workspace_snapshot_skips_files_over_the_size_limit() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("small.txt"), "ok").unwrap();
        let big = vec![b'x'; (TREE_MAX_FILE + 1) as usize];
        std::fs::write(workspace.join("big.bin"), &big).unwrap();

        let report = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert_eq!(report.files, 1, "{report:?}");
        assert_eq!(report.skipped_large, 1);
    }

    #[test]
    fn gc_removes_a_session_and_sweeps_orphans() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "pre").unwrap();
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("s1");
        store
            .record("s2", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("s2");

        assert!(store.size("s1").expect("size") > 0);
        assert!(store.remove_session("s1").expect("remove"));
        assert!(!store.remove_session("s1").expect("remove again"));

        // s2 is kept, s1 is already gone, s3 is an orphan tree.
        std::fs::create_dir_all(store.root().join("s3").join("1")).unwrap();
        let removed = store.sweep(&["s2".to_string()]).expect("sweep");
        assert_eq!(removed, vec!["s3".to_string()]);
        assert!(store.root().join("s2").exists());
        assert!(!store.root().join("s3").exists());
    }

    #[test]
    fn the_shell_flag_is_off_unless_asked_for() {
        let store = SnapshotStore::new(std::env::temp_dir().join("qcode-snapshot-flag"));
        assert!(!store.shell_enabled());
        assert!(store.with_shell(true).shell_enabled());
    }
}
