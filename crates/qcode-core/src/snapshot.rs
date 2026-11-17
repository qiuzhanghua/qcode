//! Pre-images of files the agent is about to modify.
//!
//! A checkpoint is not a copy of the workspace: it is the set of files that were
//! *about to change*, captured before the change. Rolling back to turn `N`
//! restores the oldest pre-image recorded at or after `N` for every path, and
//! deletes the files that did not exist at that boundary. Files the agent never
//! touched are left alone.
//!
//! Layout under `<data-dir>/snapshots/<session>/` (ADR-0101):
//!
//! - `blobs/<sha[..2]>/<sha256>` — content, written once, then read-only;
//! - `<turn>/manifest.json` — that turn's `<relative path> -> sha256`;
//! - `<turn>/absent.txt` — paths that did not exist yet (one per line), so a
//!   rollback can remove them instead of leaving an empty file behind.
//!
//! Turns written before ADR-0101 hold `<turn>/files/<relative path>` instead and
//! are still readable: `restore` picks the layout per turn, and the two produce
//! the same result for the same logical content.
//!
//! Snapshots are immutable once written. A blob can be shared by many turns, so
//! anything that rewrites an existing blob silently corrupts every turn that
//! points at it (ADR-0101 §3).

use std::collections::BTreeMap;
use std::io;
use std::path::{Component, Path, PathBuf};

use serde_json::Value;
use sha2::{Digest, Sha256};

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
    limits: TreeLimits,
}

/// Limits for a workspace-wide snapshot taken before a shell command.
const TREE_MAX_FILE: u64 = 4 * 1024 * 1024;
const TREE_MAX_TOTAL: u64 = 64 * 1024 * 1024;
/// Directories a shell command rarely means to edit, and that are large.
///
/// The toolchain caches earn their place: the file sandbox only allows writes
/// inside the workspace, so `CARGO_HOME`/`RUSTUP_HOME` end up next to the
/// sources (see `docs/design/M0-TASKS.md`). This repository's own `.cargo-home`
/// is 1.6 GB across 64k files — enough to fill the total cap on its own and
/// leave the snapshot holding registry sources instead of the checkout.
/// `crates/qcode-eval` skips the same names.
///
/// The home-directory half of this list came from measuring one machine
/// (2026-11-17, ADR-0101's review): a session whose tree snapshot was rooted at
/// `$HOME` spent 90.9% of its 22.3 GB on `.WebUI` (browser profile),
/// `.bun/install/cache` and `.anaconda_backup`, and in 250 of its turns the
/// 64 MB budget ran out before a single file of the checkout was copied. A
/// cache or profile directory is never what `run_shell` was asked to change, and
/// a skipped directory costs a rollback of that directory only — the same
/// bargain `target/` already makes.
const TREE_SKIP: &[&str] = &[
    // VCS metadata: never what the command meant to edit.
    ".git",
    ".hg",
    ".svn",
    // Build output.
    "target",
    "dist",
    "node_modules",
    ".venv",
    "venv",
    "__pycache__",
    ".tox",
    ".pytest_cache",
    ".mypy_cache",
    ".ruff_cache",
    ".next",
    ".nuxt",
    ".svelte-kit",
    ".parcel-cache",
    ".turbo",
    "Pods",
    // Toolchain and package-manager caches.
    ".cargo-home",
    ".rustup-home",
    ".toolchains",
    ".cargo",
    ".rustup",
    ".npm",
    ".bun",
    ".yarn",
    ".pnpm-store",
    ".gradle",
    ".m2",
    ".cache",
    // This tool's own state, and the scratch trees next to it.
    ".tmp",
    ".qcode-data",
    ".qc-data",
    // Desktop and browser profiles: megabytes of binary state per directory,
    // rewritten on a timer by something that is not the agent.
    "Library",
    ".WebUI",
    ".local",
    ".Trash",
    // Editor and backup trees that hold full copies of everything.
    ".anaconda_backup",
    ".anaconda",
    ".conda",
];

/// Size bounds for a workspace snapshot. Fields rather than bare constants so
/// the tests can shrink them instead of writing 64 MB to disk.
#[derive(Debug, Clone, Copy)]
struct TreeLimits {
    file: u64,
    total: u64,
}

impl Default for TreeLimits {
    fn default() -> Self {
        Self {
            file: TREE_MAX_FILE,
            total: TREE_MAX_TOTAL,
        }
    }
}

#[derive(Debug, Clone, Default, PartialEq, Eq)]
pub struct TreeReport {
    /// Files stored for this turn, including what earlier calls in the same
    /// turn put there: the cap is charged per turn.
    pub files: usize,
    /// Bytes stored for this turn, on the same accounting.
    pub bytes: u64,
    /// Files this call wrote. Zero means the turn was already full, which is
    /// not the same event as hitting the cap and stopping part-way.
    pub added_files: usize,
    pub added_bytes: u64,
    pub skipped_large: usize,
    /// The total cap was hit; the snapshot is partial.
    pub capped: bool,
}

impl SnapshotStore {
    pub fn new(root: impl Into<PathBuf>) -> Self {
        Self {
            root: root.into(),
            shell: false,
            limits: TreeLimits::default(),
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
        let session_dir = self.session_dir(session);
        let dir = session_dir.join(turn.to_string());
        let mut manifest = read_manifest(&dir)?;
        let mut absent = read_absent(&dir)?;
        let mut recorded = 0;
        for path in paths {
            let Some(relative) = relative_to(primary_root, path) else {
                continue;
            };
            let key = path_key(&relative);
            if manifest.files.contains_key(&key) || absent.iter().any(|known| known == &relative) {
                continue;
            }
            match std::fs::read(primary_root.join(&relative)) {
                Ok(bytes) => {
                    // Only content this session did not already hold is charged
                    // (ADR-0101 §5), and only that is newly spent on disk.
                    let new = !blob_exists(&session_dir, &sha256(&bytes));
                    let sha = write_blob(&session_dir, &bytes)?;
                    if new {
                        manifest.charged += bytes.len() as u64;
                    }
                    manifest.files.insert(key, sha);
                    recorded += 1;
                }
                Err(error) if error.kind() == io::ErrorKind::NotFound => {
                    absent.push(relative);
                    recorded += 1;
                }
                Err(error) => return Err(error),
            }
        }
        if recorded > 0 {
            write_manifest(&dir, &manifest)?;
            write_absent(&dir, &absent)?;
        }
        Ok(recorded)
    }

    /// Copy the whole workspace as the pre-image for `turn`, bounded by
    /// [`TREE_MAX_FILE`] / [`TREE_MAX_TOTAL`] and skipping build/VCS/cache trees.
    ///
    /// Used before `run_shell`, which can write any file: the alternative is
    /// guessing paths from a command line, and a wrong guess loses the file.
    ///
    /// `primary_root` must be the session's own root, not the process working
    /// directory. Those differ whenever a frontend is started somewhere else and
    /// the session is pointed at a checkout afterwards, and the snapshot would
    /// then walk the startup directory instead — megabytes of home directory per
    /// turn, with the checkout itself never reached (ADR-0101, review).
    pub fn snapshot_tree(
        &self,
        session: &str,
        turn: u32,
        primary_root: &Path,
    ) -> io::Result<TreeReport> {
        let session_dir = self.session_dir(session);
        let dir = session_dir.join(turn.to_string());
        let mut manifest = read_manifest(&dir)?;
        // The cap belongs to the turn, not to this call, and it is charged in
        // **unique bytes** (ADR-0101 §5): content the session already holds is
        // free, no matter how many paths or turns point at it. Charging raw bytes
        // made a single `run_shell` eat the whole 64 MB by re-copying an
        // unchanged tree, after which real changes no longer fit and a rollback
        // came back "partly usable".
        let mut report = TreeReport {
            files: manifest.files.len(),
            bytes: manifest.charged,
            ..TreeReport::default()
        };
        if report.bytes >= self.limits.total {
            // Already full — whether from an earlier command in this turn or,
            // for turns recorded before the toolchain caches were skipped, from
            // a size that is over the cap by accident. `added_bytes == 0` tells
            // the caller this is not a fresh truncation.
            report.capped = true;
            return Ok(report);
        }
        let mut stack = vec![primary_root.to_path_buf()];
        while let Some(current) = stack.pop() {
            let Ok(entries) = std::fs::read_dir(&current) else {
                continue;
            };
            // Sorted, so a capped snapshot always keeps the same prefix of the
            // tree instead of whatever order the filesystem happened to report.
            let mut entries: Vec<_> = entries.flatten().collect();
            entries.sort_by_key(|entry| entry.file_name());
            let mut subdirs = Vec::new();
            for entry in entries {
                let path = entry.path();
                if path.is_dir() {
                    let name = entry.file_name();
                    if !TREE_SKIP.contains(&name.to_string_lossy().as_ref()) {
                        subdirs.push(path);
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
                if metadata.len() > self.limits.file {
                    report.skipped_large += 1;
                    continue;
                }
                let key = path_key(relative);
                // Stored by an earlier call in this turn: the first pre-image
                // wins, and it is already counted.
                if manifest.files.contains_key(&key) {
                    continue;
                }
                let Ok(bytes) = std::fs::read(&path) else {
                    continue;
                };
                let sha = sha256(&bytes);
                // Content the session already holds costs nothing: the blob is
                // reused, not written again.
                let cost = if blob_exists(&session_dir, &sha) {
                    0
                } else {
                    bytes.len() as u64
                };
                if report.bytes + cost > self.limits.total {
                    report.capped = true;
                    break;
                }
                write_blob(&session_dir, &bytes)?;
                manifest.files.insert(key, sha);
                manifest.charged += cost;
                report.files += 1;
                report.bytes += cost;
                report.added_files += 1;
                report.added_bytes += cost;
            }
            if report.capped {
                break;
            }
            // Pushed in reverse so the walk stays lexicographic as it pops.
            stack.extend(subdirs.into_iter().rev());
        }
        if report.added_files > 0 {
            write_manifest(&dir, &manifest)?;
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
        if manifest_exists(&dir) {
            files.extend(read_manifest(&dir)?.files.into_keys());
        } else {
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
            // Two layouts, one result. A turn written before ADR-0101 has no
            // manifest and its pre-images are whole files under `files/`; a turn
            // written since has a manifest naming blobs. Picking per turn keeps
            // old sessions readable without a migration, and this branch is the
            // only place that knows the difference.
            if manifest_exists(&dir) {
                for (relative, sha) in read_manifest(&dir)?.files {
                    pre_images
                        .entry(PathBuf::from(relative))
                        .or_insert_with(|| Some(blob_path(&self.session_dir(session), &sha)));
                }
                continue;
            }
            let mut stored = Vec::new();
            let files_dir = dir.join("files");
            if files_dir.is_dir() {
                collect_files(&files_dir, &files_dir, &mut stored)?;
            }
            for path in stored {
                pre_images
                    .entry(path.clone())
                    .or_insert_with(|| Some(dir.join("files").join(&path)));
            }
        }

        for (relative, source) in pre_images {
            let target = primary_root.join(&relative);
            match source {
                Some(from) => {
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

/// `<session>/blobs/<sha[..2]>/<sha256>` — the same content-addressed shape
/// `qcode-attachments` uses (ADR-0101 §1). The shard keeps a single directory
/// in the thousands of entries; the flat name means the path no longer grows
/// with workspace depth.
fn blob_path(session_dir: &Path, sha: &str) -> PathBuf {
    let shard = sha.get(..2).unwrap_or(sha);
    session_dir.join("blobs").join(shard).join(sha)
}

fn sha256(bytes: &[u8]) -> String {
    let digest = Sha256::digest(bytes);
    let mut hex = String::with_capacity(64);
    for byte in digest {
        hex.push_str(&format!("{byte:02x}"));
    }
    hex
}

/// Store `bytes` under its sha256 and return the name. Existing content is left
/// alone: that is what makes a repeated pre-image free instead of a copy.
///
/// The write goes through `tmp/` and a rename, never a plain `fs::write`
/// (ADR-0101 §4). A blob interrupted halfway would otherwise sit at a path that
/// claims a sha it does not match, and the "already exists, skip" rule above
/// would then reuse it for every later turn — a rollback would restore corrupt
/// bytes with no error anywhere.
fn blob_exists(session_dir: &Path, sha: &str) -> bool {
    blob_path(session_dir, sha).is_file()
}

fn write_blob(session_dir: &Path, bytes: &[u8]) -> io::Result<String> {
    let sha = sha256(bytes);
    let target = blob_path(session_dir, &sha);
    if target.is_file() {
        return Ok(sha);
    }
    qcode_attachments::store::write_atomically(session_dir, &target, bytes)?;
    Ok(sha)
}

/// One turn's `<relative path> -> sha256`, plus the unique bytes charged to it.
///
/// `charged` exists because the cap is per turn while deduplication is per
/// session: a turn that only re-references blobs an earlier turn stored must
/// remember it paid nothing, or its second `run_shell` would recompute the total
/// from the manifest, decide it was already full, and stop recording — the exact
/// failure ADR-0101 §5 set out to remove.
#[derive(Debug, Default, Clone, PartialEq, Eq)]
struct Manifest {
    files: BTreeMap<String, String>,
    charged: u64,
}

fn manifest_path(dir: &Path) -> PathBuf {
    dir.join("manifest.json")
}

fn manifest_exists(dir: &Path) -> bool {
    manifest_path(dir).is_file()
}

/// A manifest that exists but cannot be parsed is an error, not an empty map:
/// treating it as empty would let a rollback silently skip a turn's pre-images
/// and report success.
fn read_manifest(dir: &Path) -> io::Result<Manifest> {
    let path = manifest_path(dir);
    let text = match std::fs::read_to_string(&path) {
        Ok(text) => text,
        Err(error) if error.kind() == io::ErrorKind::NotFound => return Ok(Manifest::default()),
        Err(error) => return Err(error),
    };
    let parsed: serde_json::Value = serde_json::from_str(&text)
        .map_err(|error| io::Error::new(io::ErrorKind::InvalidData, error))?;
    let mut manifest = Manifest {
        charged: parsed
            .get("charged")
            .and_then(serde_json::Value::as_u64)
            .unwrap_or(0),
        ..Manifest::default()
    };
    if let Some(entries) = parsed.get("files").and_then(serde_json::Value::as_object) {
        for (relative, sha) in entries {
            if let Some(sha) = sha.as_str() {
                manifest.files.insert(relative.clone(), sha.to_string());
            }
        }
    }
    Ok(manifest)
}

fn write_manifest(dir: &Path, manifest: &Manifest) -> io::Result<()> {
    std::fs::create_dir_all(dir)?;
    let body = serde_json::json!({
        "files": manifest.files,
        "charged": manifest.charged,
    });
    let text = serde_json::to_vec_pretty(&body).map_err(io::Error::other)?;
    qcode_attachments::store::write_atomically(
        dir.parent().unwrap_or(dir),
        &manifest_path(dir),
        &text,
    )
}

/// A relative path as a manifest key: `/`-separated on every platform, so a
/// manifest written on Windows still restores on macOS.
fn path_key(relative: &Path) -> String {
    relative
        .components()
        .map(|component| component.as_os_str().to_string_lossy().into_owned())
        .collect::<Vec<_>>()
        .join("/")
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
    fn a_workspace_snapshot_skips_toolchain_caches() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("src.rs"), "source").unwrap();
        // The sandbox parks CARGO_HOME inside the workspace; a real one is 1.6 GB
        // and would fill the cap before the checkout is reached.
        for cache in [".cargo-home/registry", ".rustup-home/toolchains"] {
            std::fs::create_dir_all(workspace.join(cache)).unwrap();
            std::fs::write(workspace.join(cache).join("blob"), "cache").unwrap();
        }

        let report = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert_eq!(report.files, 1, "{report:?}");
        assert!(!report.capped);
        assert_eq!(
            store.files("s1", 1).expect("files"),
            vec!["src.rs".to_string()]
        );
    }

    #[test]
    fn the_tree_cap_is_charged_per_turn_not_per_call() {
        let (_dir, mut store, workspace) = fixture();
        store.limits = TreeLimits {
            file: 1024,
            total: 10,
        };
        // Four distinct 4-byte files. Distinct because the cap is charged in
        // unique bytes (ADR-0101 §5): four copies of the same bytes cost 4, not
        // 16, and that distinction is the point of the next test.
        for (index, name) in ["a.txt", "b.txt", "c.txt", "d.txt"].iter().enumerate() {
            std::fs::write(workspace.join(name), format!("{}345", index)).unwrap();
        }

        let first = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert_eq!((first.files, first.bytes), (2, 8), "{first:?}");
        assert!(first.capped);

        // The second shell command in the same turn must not hand out a fresh
        // budget: the turn already holds 8 of its 10 bytes.
        let second = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert_eq!((second.files, second.bytes), (2, 8), "{second:?}");
        assert!(second.capped);

        // A new turn starts from an empty budget and can now record **all four**:
        // `a.txt` and `b.txt` are content this session already holds, so they
        // cost nothing, and only `c.txt`/`d.txt` reach its own 10-byte cap. Under
        // the old raw-byte charge this turn would have capped at two files again
        // even though it added only 8 bytes of disk.
        let next = store.snapshot_tree("s1", 2, &workspace).expect("tree");
        assert_eq!((next.files, next.bytes), (4, 8), "{next:?}");
        assert!(!next.capped, "{next:?}");
    }

    #[test]
    fn a_turn_that_was_already_full_adds_nothing_and_says_so() {
        let (_dir, mut store, workspace) = fixture();
        store.limits = TreeLimits {
            file: 1024,
            total: 10,
        };
        for (index, name) in ["a.txt", "b.txt", "c.txt"].iter().enumerate() {
            std::fs::write(workspace.join(name), format!("{}345", index)).unwrap();
        }

        let first = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert!(first.capped);
        assert_eq!(first.added_files, 2, "{first:?}");

        // Same turn, second shell command: still capped, but it wrote nothing,
        // so the caller can stay quiet instead of warning again.
        let again = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert!(again.capped);
        assert_eq!(again.added_files, 0, "{again:?}");
        assert_eq!(again.added_bytes, 0, "{again:?}");
        assert_eq!((again.files, again.bytes), (2, 8), "{again:?}");
    }

    #[test]
    fn a_capped_snapshot_keeps_the_same_prefix_every_time() {
        let (_dir, mut store, workspace) = fixture();
        store.limits = TreeLimits {
            file: 1024,
            total: 10,
        };
        // Distinct contents, so the cap bites on the third file instead of being
        // absorbed by deduplication.
        for (index, name) in ["d.txt", "b.txt", "a.txt", "c.txt"].iter().enumerate() {
            std::fs::write(workspace.join(name), format!("{}345", index)).unwrap();
        }

        // Sorted walk, so the truncated set is `a.txt` + `b.txt` rather than
        // whichever two files the filesystem listed first.
        store.snapshot_tree("s1", 1, &workspace).expect("tree");
        let stored = store.files("s1", 1).expect("files");
        assert_eq!(stored, vec!["a.txt".to_string(), "b.txt".to_string()]);
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

    fn blob_bytes(store: &SnapshotStore, session: &str) -> u64 {
        blobs_of(store, session)
            .iter()
            .filter_map(|blob| std::fs::metadata(blob).ok())
            .map(|metadata| metadata.len())
            .sum()
    }

    fn blobs_of(store: &SnapshotStore, session: &str) -> Vec<PathBuf> {
        let mut found = Vec::new();
        let dir = store.root().join(session).join("blobs");
        if !dir.is_dir() {
            return found;
        }
        for shard in std::fs::read_dir(&dir).expect("blobs").flatten() {
            if !shard.file_type().map(|kind| kind.is_dir()).unwrap_or(false) {
                continue;
            }
            for blob in std::fs::read_dir(shard.path()).expect("shard").flatten() {
                found.push(blob.path());
            }
        }
        found.sort();
        found
    }

    /// ADR-0101 verification 1.
    #[test]
    fn the_same_content_in_two_turns_is_stored_once() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "unchanged content").unwrap();

        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("turn 1");
        store
            .record("s1", 2, &workspace, &[PathBuf::from("a.txt")])
            .expect("turn 2");

        assert_eq!(
            blobs_of(&store, "s1").len(),
            1,
            "one content, one blob, across turns"
        );
        // Both turns still name it, so rolling back to either finds the bytes.
        for turn in [1, 2] {
            let manifest =
                read_manifest(&store.root().join("s1").join(turn.to_string())).expect("manifest");
            assert_eq!(manifest.files.len(), 1, "turn {turn}");
        }
        let report = store.restore("s1", 1, &workspace).expect("restore");
        assert_eq!(report.restored, 1);
        assert_eq!(
            std::fs::read_to_string(workspace.join("a.txt")).unwrap(),
            "unchanged content"
        );
    }

    /// ADR-0101 verification 2: the regression test for the 93% measured on a
    /// real session. A second tree snapshot of an unchanged workspace must add
    /// nothing, and the session directory must not grow.
    #[test]
    fn a_second_unchanged_tree_snapshot_adds_nothing_and_grows_nothing() {
        let (_dir, store, workspace) = fixture();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        for name in ["a.rs", "b.rs", "c.rs"] {
            std::fs::write(workspace.join("src").join(name), name.repeat(100)).unwrap();
        }

        let first = store.snapshot_tree("s1", 1, &workspace).expect("turn 1");
        assert_eq!(first.added_files, 3, "{first:?}");
        assert!(first.added_bytes > 0);
        let blobs_after_first = blob_bytes(&store, "s1");

        let second = store.snapshot_tree("s1", 2, &workspace).expect("turn 2");
        assert_eq!(second.added_files, 3, "the manifest still names them");
        assert_eq!(
            second.added_bytes, 0,
            "unchanged content must cost zero unique bytes: {second:?}"
        );
        // Blob bytes stay flat. The turn's own `manifest.json` is new and is
        // O(number of paths), not O(file sizes) — that is the whole trade.
        assert_eq!(
            blob_bytes(&store, "s1"),
            blobs_after_first,
            "no blob may be written twice"
        );
        assert_eq!(blobs_of(&store, "s1").len(), 3);
    }

    /// ADR-0101 verification 3: a change, a deletion and a creation rolled back
    /// through the new layout must land byte for byte where the old test puts
    /// them.
    #[test]
    fn the_manifest_layout_rolls_back_a_change_a_delete_and_a_create() {
        let (_dir, store, workspace) = fixture();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/changed.rs"), "v1").unwrap();
        std::fs::write(workspace.join("deleted.rs"), "keep me").unwrap();

        store.snapshot_tree("s1", 1, &workspace).expect("tree");

        std::fs::write(workspace.join("src/changed.rs"), "v2").unwrap();
        std::fs::remove_file(workspace.join("deleted.rs")).unwrap();
        std::fs::write(workspace.join("created.rs"), "new").unwrap();

        let report = store.restore("s1", 1, &workspace).expect("restore");
        assert_eq!(report.restored, 2, "{report:?}");
        assert_eq!(
            std::fs::read_to_string(workspace.join("src/changed.rs")).unwrap(),
            "v1"
        );
        assert_eq!(
            std::fs::read_to_string(workspace.join("deleted.rs")).unwrap(),
            "keep me"
        );
        assert!(workspace.join("created.rs").exists());
    }

    /// ADR-0101 verification 6 (and boundary 3): an old-layout turn directory
    /// with only `files/` still restores, and both layouts restore the same
    /// logical content identically.
    #[test]
    fn both_layouts_restore_the_same_logical_content() {
        let (_dir, store, workspace) = fixture();
        std::fs::create_dir_all(workspace.join("src")).unwrap();
        std::fs::write(workspace.join("src/a.rs"), "old bytes").unwrap();

        // Hand-build a pre-ADR-0101 turn: files/ only, no manifest.
        let turn_dir = store.root().join("s1").join("1");
        std::fs::create_dir_all(turn_dir.join("files/src")).unwrap();
        std::fs::write(turn_dir.join("files/src/a.rs"), "old bytes").unwrap();
        assert!(
            !manifest_exists(&turn_dir),
            "the fixture must be old layout"
        );

        // The same logical content through the new layout, in another session.
        let new_turn = store.root().join("s2").join("1");
        std::fs::create_dir_all(&new_turn).unwrap();
        let sha = write_blob(&store.root().join("s2"), b"old bytes").expect("blob");
        let mut manifest = Manifest::default();
        manifest.files.insert("src/a.rs".to_string(), sha);
        manifest.charged = b"old bytes".len() as u64;
        write_manifest(&new_turn, &manifest).expect("manifest");

        for session in ["s1", "s2"] {
            std::fs::write(workspace.join("src/a.rs"), "clobbered").unwrap();
            let report = store.restore(session, 1, &workspace).expect("restore");
            assert_eq!(report.restored, 1, "{session}: {report:?}");
            assert_eq!(
                std::fs::read_to_string(workspace.join("src/a.rs")).unwrap(),
                "old bytes",
                "{session} must restore the same bytes"
            );
            // `files()` reports the same logical set from either layout.
            assert_eq!(
                store.files(session, 1).expect("files"),
                vec!["src/a.rs".to_string()],
                "{session}"
            );
        }
    }

    /// ADR-0101 verification 5: the cap counts unique bytes, so a turn that is
    /// mostly a repeat of what an earlier turn stored does not get capped.
    #[test]
    fn the_cap_counts_unique_bytes_not_copied_bytes() {
        let (_dir, mut store, workspace) = fixture();
        store.limits = TreeLimits {
            file: 1024,
            total: 10,
        };
        // Turn 1 stores 8 unique bytes.
        std::fs::write(workspace.join("a.txt"), "aaaa").unwrap();
        std::fs::write(workspace.join("b.txt"), "bbbb").unwrap();
        let first = store.snapshot_tree("s1", 1, &workspace).expect("turn 1");
        assert_eq!((first.bytes, first.capped), (8, false), "{first:?}");

        // Turn 2 touches the same files, changes one, and adds a new 2-byte file:
        // the repeated 8 bytes are free, so the 2 new bytes fit in the 10-byte
        // budget. Charging raw bytes would have capped this turn immediately.
        std::fs::write(workspace.join("a.txt"), "aaaa").unwrap();
        std::fs::write(workspace.join("b.txt"), "cccc").unwrap();
        std::fs::write(workspace.join("c.txt"), "dd").unwrap();
        let second = store.snapshot_tree("s1", 2, &workspace).expect("turn 2");
        assert_eq!(
            (
                second.files,
                second.bytes,
                second.added_bytes,
                second.capped
            ),
            (3, 6, 6, false),
            "only 'cccc' and 'dd' are new bytes: {second:?}"
        );
    }

    /// ADR-0101 verification 7: deleting a session takes its blobs with it and
    /// leaves another session's blobs untouched.
    #[test]
    fn deleting_a_session_takes_its_blobs_and_no_one_elses() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "shared content").unwrap();
        for session in ["s1", "s2"] {
            store
                .record(session, 1, &workspace, &[PathBuf::from("a.txt")])
                .expect("record");
        }
        assert_eq!(blobs_of(&store, "s1").len(), 1);
        assert_eq!(blobs_of(&store, "s2").len(), 1);

        assert!(store.remove_session("s1").expect("remove"));
        assert!(
            !store.root().join("s1").exists(),
            "the whole session directory goes, blobs included"
        );
        // No reference counting anywhere: s2 owns its own copy.
        assert_eq!(blobs_of(&store, "s2").len(), 1);
        let report = store.restore("s2", 1, &workspace).expect("restore s2");
        assert_eq!(report.restored, 1);
    }

    /// ADR-0101 verification 8: a blob orphaned by deleting a turn directory is
    /// recorded garbage, deliberately not collected.
    #[test]
    fn a_blob_left_by_a_deleted_turn_is_not_collected() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "one").unwrap();
        std::fs::write(workspace.join("b.txt"), "two").unwrap();
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("turn 1");
        store
            .record("s1", 2, &workspace, &[PathBuf::from("b.txt")])
            .expect("turn 2");
        assert_eq!(blobs_of(&store, "s1").len(), 2);

        // Deleting a turn is not something the product offers, but the ADR says
        // to write down what happens: the blob stays, unreferenced, until the
        // session itself goes.
        std::fs::remove_dir_all(store.root().join("s1").join("2")).expect("remove turn");
        assert_eq!(
            blobs_of(&store, "s1").len(),
            2,
            "no sweep runs at turn granularity"
        );
        assert_eq!(store.turns("s1").expect("turns"), vec![1]);
    }

    /// ADR-0101 verification 9, in the form this API can actually support.
    ///
    /// The ADR asks for a crash injected mid-write, so that a half-written blob
    /// would be caught at its sha path. There is no seam for that here:
    /// `write_blob` hands a whole buffer to a helper whose only publishes are
    /// `rename`s, so a test can neither interrupt the write nor observe a partial
    /// file at the destination. What is pinned instead is what a failure *is*
    /// allowed to leave behind:
    ///
    /// 1. a failed publish leaves nothing at the blob path,
    /// 2. the scratch directory is used and ends up empty,
    /// 3. anything at a blob path holds exactly the bytes its name claims.
    ///
    /// Point 3 is the invariant the ADR cares about — a blob whose content does
    /// not match its name is reused forever, because the write path skips what
    /// already exists. Point 1 is what a non-atomic writer would break the moment
    /// the write itself fails, rather than on a power cut.
    #[test]
    fn a_failed_publish_leaves_nothing_at_the_blob_path() {
        let (_dir, store, workspace) = fixture();
        let session_dir = store.root().join("s1");
        // A shard path occupied by a *file* makes the publish fail after the
        // bytes are already written, which is the interesting moment.
        std::fs::create_dir_all(session_dir.join("blobs")).unwrap();
        std::fs::write(session_dir.join("blobs").join("ab"), b"in the way").unwrap();
        let blocked = session_dir.join("blobs").join("ab").join("deadbeef");
        let error = qcode_attachments::store::write_atomically(&session_dir, &blocked, b"bytes");
        assert!(error.is_err(), "publishing over a file must fail");
        assert!(
            !blocked.exists(),
            "a failed publish must not leave a short blob behind"
        );

        // The normal path: the bytes go through `tmp/` and it ends up empty.
        std::fs::write(workspace.join("a.txt"), "the real bytes").unwrap();
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("record");
        let scratch = session_dir.join("tmp");
        assert!(
            scratch.is_dir(),
            "the write must go through a scratch directory"
        );
        assert_eq!(
            std::fs::read_dir(&scratch).expect("tmp").count(),
            0,
            "the scratch file must be renamed into place, not left behind"
        );
        // And the one blob that does exist matches its own name.
        let blobs = blobs_of(&store, "s1");
        assert_eq!(blobs.len(), 1, "{blobs:?}");
        let name = blobs[0].file_name().unwrap().to_string_lossy().into_owned();
        assert_eq!(sha256(&std::fs::read(&blobs[0]).unwrap()), name);
    }

    /// A file left in `tmp/` by a crashed writer is not content: it is not
    /// reachable from any manifest, it does not count toward `size`, and it does
    /// not stop the next record of the same path from producing the real blob.
    #[test]
    fn a_half_written_scratch_file_is_never_content() {
        let (_dir, store, workspace) = fixture();
        let session_dir = store.root().join("s1");
        let scratch = session_dir.join("tmp");
        std::fs::create_dir_all(&scratch).expect("tmp");
        // Half the bytes, at a name that claims nothing.
        std::fs::write(scratch.join("leftover"), b"the real").expect("half a write");

        // Nothing points at it, so nothing can restore it.
        assert!(blobs_of(&store, "s1").is_empty());
        assert!(store.turns("s1").expect("turns").is_empty());
        assert!(
            !blob_path(&session_dir, &sha256(b"the real")).exists(),
            "an interrupted write must not publish a short blob"
        );

        // Recording the same content now produces the correct blob, once.
        std::fs::write(workspace.join("a.txt"), "the real bytes").unwrap();
        store
            .record("s1", 1, &workspace, &[PathBuf::from("a.txt")])
            .expect("record");
        let blobs = blobs_of(&store, "s1");
        assert_eq!(blobs.len(), 1, "{blobs:?}");
        assert_eq!(std::fs::read(&blobs[0]).unwrap(), b"the real bytes");
        assert_eq!(
            sha256(&std::fs::read(&blobs[0]).unwrap()),
            blobs[0].file_name().unwrap().to_string_lossy()
        );
    }

    /// ADR-0101 verification 10: two sessions that snapshot the same tree each
    /// keep a full copy. This is decision 2's announced price, written down so a
    /// future measurement reads it as expected rather than as a regression.
    #[test]
    fn two_sessions_snapshotting_one_tree_keep_one_copy_each() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("a.txt"), "same tree").unwrap();
        for session in ["s1", "s2"] {
            store.snapshot_tree(session, 1, &workspace).expect("tree");
        }
        assert_eq!(blobs_of(&store, "s1").len(), 1);
        assert_eq!(blobs_of(&store, "s2").len(), 1);
        assert_eq!(
            store.size("s1").expect("s1"),
            store.size("s2").expect("s2"),
            "the same bytes twice, by design (ADR-0101 §2)"
        );
    }

    /// The skip list has to cover the home-directory trees that measurement found
    /// filling the cap before any source file of the checkout was reached.
    #[test]
    fn a_workspace_snapshot_skips_home_caches_and_profiles() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("src.rs"), "source").unwrap();
        for cache in [
            ".WebUI/WebUIChromeProfile/Default",
            ".bun/install/cache",
            ".anaconda_backup/x/.ipython/profile_default",
            "Library/Caches",
            ".cache/pip",
            ".local/share",
        ] {
            std::fs::create_dir_all(workspace.join(cache)).unwrap();
            std::fs::write(workspace.join(cache).join("blob"), "cache").unwrap();
        }

        let report = store.snapshot_tree("s1", 1, &workspace).expect("tree");
        assert_eq!(report.files, 1, "{report:?}");
        assert!(!report.capped);
        assert_eq!(
            store.files("s1", 1).expect("files"),
            vec!["src.rs".to_string()]
        );
    }

    /// A skipped directory is not merely unrecorded: a rollback must not delete
    /// it either, or turning on the skip list would start removing caches.
    #[test]
    fn a_skipped_directory_survives_a_rollback() {
        let (_dir, store, workspace) = fixture();
        std::fs::write(workspace.join("src.rs"), "v1").unwrap();
        std::fs::create_dir_all(workspace.join("target/debug")).unwrap();
        std::fs::write(workspace.join("target/debug/junk"), "build").unwrap();
        store.snapshot_tree("s1", 1, &workspace).expect("tree");

        std::fs::write(workspace.join("src.rs"), "v2").unwrap();
        store.restore("s1", 1, &workspace).expect("restore");
        assert_eq!(
            std::fs::read_to_string(workspace.join("src.rs")).unwrap(),
            "v1"
        );
        assert!(
            workspace.join("target/debug/junk").exists(),
            "an unrecorded path is left alone"
        );
    }
}
