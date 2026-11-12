use super::*;
use std::io;
use std::io::{Seek as _, Write as _};

/// How often `--follow` looks for new bytes.
///
/// Polling, not `inotify`. Notification is precise but fails exactly where it
/// matters most: network filesystems, containers whose log directory is a bind
/// mount, and editors that rewrite files. A 200ms poll costs one `metadata` call
/// per tick on a machine that is already printing, and it behaves the same
/// everywhere. The cost is up to 200ms of latency on a line that is not going
/// anywhere.
const FOLLOW_INTERVAL: Duration = Duration::from_millis(200);

/// `qc logs`: say where the log is, then show its tail (and optionally follow).
///
/// The path comes first and unconditionally, because that is what a bug report
/// needs ("attach this file") and what a user cannot guess — the file lives in the
/// data directory, which is not somewhere they browse. Tailing is the convenience
/// on top; `--path-only` turns it off for scripts that just want the name.
pub(crate) fn cmd_logs(
    args: LogsArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let data_dir = qcode_kernel::resolve_data_dir(data_dir);
    let dir = logging::log_dir(&data_dir);
    let current = logging::current_log(&data_dir);

    println!("{}", current.display());

    if args.path_only {
        return Ok(ExitCode::SUCCESS);
    }

    let files = logging::log_files(&data_dir);
    let mut follower = Follower::new(data_dir.clone());

    if files.is_empty() {
        // Not an error: file logging may never have run in this data directory,
        // and a first `qc logs` before the first turn is a normal thing to do.
        if !args.follow {
            println!("no log files in {}", dir.display());
            return Ok(ExitCode::SUCCESS);
        }
        // Following a file that does not exist yet is the whole point of pairing
        // this with a fresh `qc gui`: start the watcher first, then reproduce.
        println!(
            "waiting for the first line in {} (Ctrl-C to stop)",
            dir.display()
        );
    } else {
        // Naming the older files is the point of rotating them: a bug that showed
        // up last week is in a different file than today's.
        if files.len() > 1 {
            println!("(older files in {}: {})", dir.display(), files.len() - 1);
        }
        let text = std::fs::read_to_string(&current)
            .map_err(|error| format!("cannot read {}: {error}", current.display()))?;
        let skip = text.lines().count().saturating_sub(args.lines);
        for line in text.lines().skip(skip) {
            println!("{line}");
        }
        // Everything shown so far must not be shown again by the first poll.
        follower.skip_to_end();
    }

    if !args.follow {
        return Ok(ExitCode::SUCCESS);
    }

    follow_forever(&mut follower)?;
    Ok(ExitCode::SUCCESS)
}

/// Print whatever arrives, until the process is interrupted.
///
/// Ctrl-C ends the process the ordinary way (no handler is installed here), which
/// is what `tail -f` does and what a reader expects.
fn follow_forever(follower: &mut Follower) -> Result<(), Box<dyn std::error::Error>> {
    let mut out = io::stdout().lock();
    loop {
        match follower.poll() {
            Ok(update) => {
                if let Some(notice) = update.notice {
                    // `writeln!` and not `println!`: a closed pipe (`qc logs -f |
                    // head`) must end the command quietly, not panic inside the
                    // stdlib's own print machinery.
                    if writeln!(out, "{notice}").is_err() {
                        return Ok(());
                    }
                }
                if !update.text.is_empty() {
                    if write!(out, "{}", update.text).is_err() {
                        return Ok(());
                    }
                    // Flush per poll: the reader is a human or a pipe, not a
                    // buffered file, and a line that sits in a buffer for 200ms
                    // looks exactly like a bug.
                    if out.flush().is_err() {
                        return Ok(());
                    }
                }
            }
            Err(error) => {
                // A log that cannot be read is worth saying once; carrying on is
                // better than dying, because the next rotation may fix it.
                tracing::debug!(%error, "cannot read the log file while following");
            }
        }
        std::thread::sleep(FOLLOW_INTERVAL);
    }
}

/// What one [`Follower::poll`] found.
#[derive(Debug, Default, PartialEq, Eq)]
pub(crate) struct Update {
    /// Set when the file this follower was reading is no longer the one it must
    /// read — rotation or truncation. Printed once, as a separator.
    pub(crate) notice: Option<String>,
    /// New complete lines, with their trailing newlines.
    pub(crate) text: String,
}

/// Reads bytes appended to the current log, surviving rotation and truncation.
///
/// Split out from the loop above so it can be tested: the interesting behaviour is
/// entirely in what happens *between* two reads (a new day, a smaller file, a file
/// that appears late), and none of that is reachable by running the binary.
pub(crate) struct Follower {
    data_dir: PathBuf,
    path: PathBuf,
    /// Byte offset already printed. Always on a line boundary — see [`Self::poll`].
    offset: u64,
}

impl Follower {
    pub(crate) fn new(data_dir: PathBuf) -> Self {
        let path = logging::current_log(&data_dir);
        Self {
            data_dir,
            path,
            offset: 0,
        }
    }

    /// Pretend everything in the current file has been shown already.
    fn skip_to_end(&mut self) {
        self.offset = std::fs::metadata(&self.path).map(|m| m.len()).unwrap_or(0);
    }

    /// Collect bytes appended since the last call.
    ///
    /// Three things can change under a follower, and each has to be detected from
    /// the filesystem alone:
    ///
    /// - **A new file appears** (`current_log` resolves differently). Follow it from
    ///   the start: it is a whole new day's log.
    /// - **The same path now has a shorter file** (rotation behind the `qc.log`
    ///   symlink, or a truncating writer). Our offset points past its end, and
    ///   continuing from there would read nothing forever.
    /// - **Nothing changed.** The common case, and it must stay free of
    ///   allocations: this runs five times a second.
    pub(crate) fn poll(&mut self) -> io::Result<Update> {
        let mut notice = None;

        // Resolve every time: rotation moves the symlink target, and on platforms
        // without symlinks `current_log` picks a different dated file.
        let current = logging::current_log(&self.data_dir);
        if current != self.path {
            notice = Some(format!(
                "--- following {} ---",
                current
                    .file_name()
                    .map(|name| name.to_string_lossy().into_owned())
                    .unwrap_or_else(|| current.display().to_string())
            ));
            self.path = current;
            self.offset = 0;
        }

        let Ok(metadata) = std::fs::metadata(&self.path) else {
            // Not there yet (`--follow` before the first line was ever written).
            return Ok(Update {
                notice,
                text: String::new(),
            });
        };
        let len = metadata.len();
        if len < self.offset {
            // Rotated behind a stable path, or truncated in place. Either way the
            // bytes we counted are gone, so start over — and say so, because a log
            // that silently restarts is a log that lies about its own timeline.
            notice = Some(format!(
                "--- {} was replaced (rotated or truncated); following from the start ---",
                self.path.display()
            ));
            self.offset = 0;
        }
        if len == self.offset {
            return Ok(Update {
                notice,
                text: String::new(),
            });
        }

        let mut file = std::fs::File::open(&self.path)?;
        file.seek(io::SeekFrom::Start(self.offset))?;
        let mut bytes = Vec::new();
        file.read_to_end(&mut bytes)?;

        // Consume only whole lines. A write can land mid-line, and stopping at the
        // last newline guarantees two things: no partial line is printed, and no
        // multi-byte character is split across two reads (which would turn a CJK
        // message into replacement characters).
        let complete = bytes
            .iter()
            .rposition(|byte| *byte == b'\n')
            .map(|end| end + 1)
            .unwrap_or(0);
        self.offset += complete as u64;

        Ok(Update {
            notice,
            text: String::from_utf8_lossy(&bytes[..complete]).into_owned(),
        })
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A data dir with one dated log, so `current_log` finds it without a symlink
    /// (which is how the tests stay platform-neutral — Windows refuses to make one).
    fn fixture(dated: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let logs = logging::log_dir(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        let path = logs.join(dated);
        std::fs::write(&path, "").unwrap();
        (dir, path)
    }

    /// The common case, and the one that must not repeat itself: appending shows
    /// only the new bytes, and polling again with no write shows nothing.
    #[test]
    fn appended_lines_are_shown_once() {
        let (dir, path) = fixture("qc.2026-01-01.log");
        let mut follower = Follower::new(dir.path().to_path_buf());

        append(&path, "first\n");
        let update = follower.poll().unwrap();
        assert_eq!(update.text, "first\n");
        assert_eq!(update.notice, None);

        // Nothing new: the second poll must be silent, or `--follow` would print
        // the whole file again five times a second.
        assert_eq!(follower.poll().unwrap(), Update::default());

        append(&path, "second\n");
        assert_eq!(follower.poll().unwrap().text, "second\n");
    }

    /// A write can land mid-line. Printing the fragment would produce a line that
    /// is not in the file, and holding back half of a multi-byte character would
    /// print replacement characters instead of the message.
    #[test]
    fn a_partial_line_waits_for_its_newline() {
        let (dir, path) = fixture("qc.2026-01-01.log");
        let mut follower = Follower::new(dir.path().to_path_buf());

        append(&path, "complete\nhalf");
        assert_eq!(follower.poll().unwrap().text, "complete\n");
        assert_eq!(follower.poll().unwrap().text, "");

        append(&path, " done\n");
        assert_eq!(follower.poll().unwrap().text, "half done\n");
    }

    /// A CJK message split across two writes must survive: bytes are buffered
    /// until the newline, so no character is ever cut in half.
    #[test]
    fn a_multibyte_character_is_never_split() {
        let (dir, path) = fixture("qc.2026-01-01.log");
        let mut follower = Follower::new(dir.path().to_path_buf());

        let line = "错误：连接被拒绝\n";
        // Split inside the 3-byte `连`, which is exactly what a socket read can do.
        let bytes = line.as_bytes();
        let cut = 9 + 1;
        assert!(
            !line.is_char_boundary(cut),
            "the test wants a mid-character cut"
        );
        // Raw bytes, not `append`: a partial character has no `&str` form, and that
        // is exactly the shape a real short write has.
        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&bytes[..cut])
            .unwrap();
        assert_eq!(
            follower.poll().unwrap().text,
            "",
            "half a character printed"
        );

        std::fs::OpenOptions::new()
            .append(true)
            .open(&path)
            .unwrap()
            .write_all(&bytes[cut..])
            .unwrap();
        assert_eq!(follower.poll().unwrap().text, line);
    }

    /// Rotation on a platform without symlinks swaps the resolved path. The new
    /// file is a new day's log, so it is read from the start and announced.
    #[test]
    fn rotation_to_a_new_file_is_announced_and_read_from_the_start() {
        let (dir, path) = fixture("qc.2026-01-01.log");
        let mut follower = Follower::new(dir.path().to_path_buf());
        append(&path, "yesterday\n");
        assert_eq!(follower.poll().unwrap().text, "yesterday\n");

        let next = logging::log_dir(dir.path()).join("qc.2026-01-02.log");
        std::fs::write(&next, "today\n").unwrap();
        // Make the old file older, because `log_files` orders by modification time
        // and the two writes above can land in the same clock tick.
        touch(&path, std::time::SystemTime::UNIX_EPOCH);

        let update = follower.poll().unwrap();
        assert!(
            update
                .notice
                .unwrap()
                .contains("following qc.2026-01-02.log")
        );
        assert_eq!(update.text, "today\n");
        assert_eq!(follower.poll().unwrap(), Update::default());
    }

    /// Rotation behind a stable path (the `qc.log` symlink) cannot be detected by
    /// the name, so it shows up as the file being shorter than what was printed.
    /// Reading past the end would stall forever.
    #[test]
    fn a_shorter_file_restarts_from_the_beginning() {
        let (dir, path) = fixture("qc.2026-01-01.log");
        let mut follower = Follower::new(dir.path().to_path_buf());
        append(&path, "a long line from the previous file\n");
        follower.poll().unwrap();

        // Same path, new contents, fewer bytes.
        std::fs::write(&path, "short\n").unwrap();
        let update = follower.poll().unwrap();
        assert!(update.notice.unwrap().contains("replaced"));
        assert_eq!(update.text, "short\n");
    }

    /// `qc logs -f` in one terminal before the first `qc gui` line is written. The
    /// file does not exist yet, and that must not be an error.
    #[test]
    fn a_file_that_appears_later_is_picked_up() {
        let dir = tempfile::tempdir().unwrap();
        std::fs::create_dir_all(logging::log_dir(dir.path())).unwrap();
        let mut follower = Follower::new(dir.path().to_path_buf());

        assert_eq!(follower.poll().unwrap(), Update::default());
        assert_eq!(follower.poll().unwrap(), Update::default());

        let path = logging::log_dir(dir.path()).join("qc.2026-01-01.log");
        std::fs::write(&path, "first ever line\n").unwrap();
        assert_eq!(follower.poll().unwrap().text, "first ever line\n");
    }

    /// The tail is printed by `cmd_logs` and then `skip_to_end` runs; if that did
    /// not move the offset, the first poll would repeat the whole file.
    #[test]
    fn skip_to_end_does_not_repeat_what_was_already_shown() {
        let (dir, path) = fixture("qc.2026-01-01.log");
        append(&path, "already shown\n");
        let mut follower = Follower::new(dir.path().to_path_buf());
        follower.skip_to_end();

        assert_eq!(follower.poll().unwrap(), Update::default());
        append(&path, "brand new\n");
        assert_eq!(follower.poll().unwrap().text, "brand new\n");
    }

    fn append(path: &std::path::Path, text: &str) {
        let mut file = std::fs::OpenOptions::new().append(true).open(path).unwrap();
        file.write_all(text.as_bytes()).unwrap();
    }

    fn touch(path: &std::path::Path, when: std::time::SystemTime) {
        std::fs::File::options()
            .write(true)
            .open(path)
            .unwrap()
            .set_modified(when)
            .unwrap();
    }
}
