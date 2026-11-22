use super::*;
use std::borrow::Cow;
use std::collections::VecDeque;
use std::io;
use std::io::{Seek as _, Write as _};
use std::path::Path;

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
    let dir = qcode_kernel::logging::log_dir(&data_dir);
    let current = qcode_kernel::logging::current_log(&data_dir);

    println!("{}", current.display());

    if args.path_only {
        return Ok(ExitCode::SUCCESS);
    }

    // Built before anything is read, so a bad pattern is one clear error rather
    // than a filter that silently passes everything.
    let filter = Filter::new(args.grep.as_deref(), args.since.as_deref())?;

    let files = match qcode_kernel::logging::log_files(&data_dir) {
        Ok(files) => files,
        // Not "no log files": the directory could not be listed, and saying it is
        // empty sends the reader looking for a logging bug that is not there. An
        // unreadable directory is its own answer, and it is a failure.
        Err(error) => {
            return Err(qcode_i18n::trp(
                "err.cli.logs.cannot_read",
                &[
                    dir.display().to_string().as_str(),
                    error.to_string().as_str(),
                ],
            )
            .into());
        }
    };
    let mut follower = Follower::new(data_dir.clone());

    if files.is_empty() {
        // Not an error: file logging may never have run in this data directory,
        // and a first `qc logs` before the first turn is a normal thing to do.
        if !args.follow {
            println!(
                "{}",
                qcode_i18n::trp("cli.logs.no_files", &[dir.display().to_string().as_str()])
            );
            return Ok(ExitCode::SUCCESS);
        }
        // Following a file that does not exist yet is the whole point of pairing
        // this with a fresh `qc gui`: start the watcher first, then reproduce.
        println!(
            "{}",
            qcode_i18n::trp("cli.logs.waiting", &[dir.display().to_string().as_str()])
        );
    } else {
        // Naming the older files is the point of rotating them: a bug that showed
        // up last week is in a different file than today's.
        if files.len() > 1 {
            if filter.is_active() {
                // Said differently when filtering, because it is then a promise and
                // not just a note: the search below really does open all of them.
                // The old wording ("older files in …: 3") sat next to a `--grep` that
                // only read the current file, so a session that started before the
                // rotation looked like it had never started.
                println!(
                    "{}",
                    qcode_i18n::trp(
                        "cli.logs.searching",
                        &[
                            files.len().to_string().as_str(),
                            dir.display().to_string().as_str()
                        ]
                    )
                );
            } else {
                println!(
                    "{}",
                    qcode_i18n::trp(
                        "cli.logs.older",
                        &[
                            dir.display().to_string().as_str(),
                            (files.len() - 1).to_string().as_str()
                        ]
                    )
                );
            }
        }
        if filter.is_active() {
            // A question about *a line* spans files; the tail of today's file does
            // not. This is the case that produced a wrong answer: `qc logs --grep
            // 'pid=14275'` printed only `session ending`, because the `starting` line
            // had gone to the previous day's file at the UTC-midnight rotation (08:00
            // in CST) while the session was still running.
            for line in filter.tail_across(&files, &current, args.lines)? {
                println!("{}", local_time(&line));
            }
        } else {
            let text = std::fs::read_to_string(&current)
                .map_err(|error| format!("cannot read {}: {error}", current.display()))?;
            let skip = text.lines().count().saturating_sub(args.lines);
            for line in text.lines().skip(skip) {
                println!("{}", local_time(line));
            }
        }
        // Everything shown so far must not be shown again by the first poll.
        follower.skip_to_end();
    }

    if !args.follow {
        return Ok(ExitCode::SUCCESS);
    }

    follow_forever(&mut follower, &filter)?;
    Ok(ExitCode::SUCCESS)
}

/// What `qc logs` shows: an optional pattern and an optional lower time bound.
///
/// Why this exists rather than leaving it to `grep`: even with third-party noise
/// kept out of the file (ADR-0093), a GUI start writes its own hundred lines around
/// the two that say how the run ended. Those are the reason to open the file at all,
/// and finding them meant knowing where to look. Here it is one flag — and it works
/// while following, which `grep` cannot do.
#[derive(Debug)]
pub(crate) struct Filter {
    pattern: Option<regex::Regex>,
    since: Option<chrono::DateTime<chrono::FixedOffset>>,
}

impl Filter {
    pub(crate) fn new(
        pattern: Option<&str>,
        since: Option<&str>,
    ) -> Result<Self, Box<dyn std::error::Error>> {
        let pattern = match pattern {
            Some(source) => Some(regex::Regex::new(source).map_err(|error| {
                // Named rather than passed through: `regex`'s own message starts
                // with the pattern, and from there it is not obvious which flag
                // produced it.
                format!("--grep {source:?} is not a valid pattern: {error}")
            })?),
            None => None,
        };
        Ok(Self {
            pattern,
            since: since.map(parse_since).transpose()?,
        })
    }

    /// Whether this filter asks a question that spans files.
    ///
    /// `false` means "tail today's file", where older files are only mentioned.
    /// `true` means "find the lines that match", and a line can be in any of them.
    fn is_active(&self) -> bool {
        self.pattern.is_some() || self.since.is_some()
    }

    /// The last `lines` matches across every file rotation left behind.
    ///
    /// Ordered by file (oldest first) and then by position, so the output reads like
    /// one continuous log. `lines` is the total, not a per-file limit: `-n 40` means
    /// forty lines, which is what someone reading a rotation boundary expects.
    ///
    /// A file that cannot be read is reported and skipped rather than taking the
    /// whole answer down with it — the older files are the ones most likely to have
    /// been touched by something else. The *current* file is different: it is what
    /// was asked for, so failing on it is still an error.
    fn tail_across(
        &self,
        files: &[PathBuf],
        current: &Path,
        lines: usize,
    ) -> Result<Vec<String>, Box<dyn std::error::Error>> {
        let mut kept: VecDeque<String> = VecDeque::with_capacity(lines.min(1024));
        // `files` holds the dated files rotation wrote; a platform without symlinks
        // names one of them `qc.log`, which `log_files` still matches, so the current
        // file is covered here. The fallback is for a directory where nothing matched
        // the pattern but `current` exists anyway.
        let sources: Vec<&Path> = if files.is_empty() {
            vec![current]
        } else {
            files.iter().map(PathBuf::as_path).collect()
        };
        for path in sources {
            let text = match std::fs::read_to_string(path) {
                Ok(text) => text,
                Err(error) if path == current => {
                    return Err(qcode_i18n::trp(
                        "err.cli.logs.cannot_read",
                        &[
                            path.display().to_string().as_str(),
                            error.to_string().as_str(),
                        ],
                    )
                    .into());
                }
                Err(error) => {
                    eprintln!(
                        "{}",
                        qcode_i18n::trp(
                            "cli.logs.skipping",
                            &[
                                path.display().to_string().as_str(),
                                error.to_string().as_str()
                            ]
                        )
                    );
                    continue;
                }
            };
            for line in text.lines().filter(|line| self.matches(line)) {
                // Bounded so `--grep .` over a week of logs does not hold the whole
                // week in memory to print forty lines of it.
                if lines == 0 {
                    return Ok(Vec::new());
                }
                if kept.len() == lines {
                    kept.pop_front();
                }
                kept.push_back(line.to_string());
            }
        }
        Ok(kept.into())
    }

    /// Whether a rendered line passes.
    ///
    /// A line with no parsable leading timestamp is dropped when `--since` is set:
    /// it cannot be placed in time, and showing it would either contradict the
    /// bound or need a second rule. The separator lines `--follow` prints (`--- …`)
    /// are exempt, because they are the command talking, not the log.
    pub(crate) fn matches(&self, line: &str) -> bool {
        if let Some(pattern) = &self.pattern
            && !pattern.is_match(line)
        {
            return false;
        }
        match &self.since {
            None => true,
            Some(bound) => match line_timestamp(line) {
                Some(stamp) => stamp >= *bound,
                None => line.starts_with("---"),
            },
        }
    }
}

/// The timestamp a log line starts with, if it has one.
fn line_timestamp(line: &str) -> Option<chrono::DateTime<chrono::FixedOffset>> {
    let (stamp, _) = line.split_once(' ')?;
    chrono::DateTime::parse_from_rfc3339(stamp).ok()
}

/// `--since`: a duration back from now (`30m`, `2h`, `7d`, `90s`) or an exact
/// RFC 3339 timestamp.
///
/// Two forms because the two questions are different: "what happened in the last
/// half hour" is how a user retells a bug, and "since 09:12" is what a log line in
/// a bug report says.
fn parse_since(
    value: &str,
) -> Result<chrono::DateTime<chrono::FixedOffset>, Box<dyn std::error::Error>> {
    let value = value.trim();
    if let Ok(exact) = chrono::DateTime::parse_from_rfc3339(value) {
        return Ok(exact);
    }
    let (amount, unit) = value.split_at(
        value
            .find(|c: char| !c.is_ascii_digit())
            .unwrap_or(value.len()),
    );
    let amount: i64 = amount.parse().map_err(|_| {
        format!("--since {value:?} is neither a duration (30m, 2h, 7d) nor an RFC 3339 timestamp")
    })?;
    let unit = unit.trim();
    let seconds = match unit {
        "s" | "sec" | "secs" => 1,
        "m" | "min" | "mins" => 60,
        "h" | "hr" | "hrs" => 3600,
        "d" | "day" | "days" => 86_400,
        "w" | "week" | "weeks" => 7 * 86_400,
        // Bare digits are seconds: the shortest thing to type for "just now".
        "" => 1,
        other => {
            return Err(qcode_i18n::trp(
                "err.cli.logs.since_not_a_unit",
                &[format!("{value:?}").as_str(), format!("{other:?}").as_str()],
            )
            .into());
        }
    };
    // Checked, not `amount * seconds`: `--since 9223372036854775807d` overflowed
    // and panicked in a debug build (`attempt to multiply with overflow`) and wrapped
    // to a nonsense instant in a release build. Neither is an answer to a bad number.
    let seconds = amount.checked_mul(seconds).ok_or_else(|| {
        qcode_i18n::trp(
            "err.cli.logs.since_too_large",
            &[format!("{value:?}").as_str()],
        )
    })?;
    let now = chrono::Local::now().fixed_offset();
    chrono::Duration::try_seconds(seconds)
        .and_then(|back| now.checked_sub_signed(back))
        .ok_or_else(|| {
            Box::<dyn std::error::Error>::from(qcode_i18n::trp(
                "err.cli.logs.since_too_far_back",
                &[format!("{value:?}").as_str()],
            ))
        })
}

/// Print whatever arrives, until the process is interrupted.
///
/// Ctrl-C ends the process the ordinary way (no handler is installed here), which
/// is what `tail -f` does and what a reader expects.
fn follow_forever(
    follower: &mut Follower,
    filter: &Filter,
) -> Result<(), Box<dyn std::error::Error>> {
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
                    // Filtered while following too, through the same helper the
                    // tail uses: `--grep` must not stop applying the moment the
                    // first new line arrives.
                    let block = rendered_block(&update.text, filter);
                    if block.is_empty() {
                        std::thread::sleep(FOLLOW_INTERVAL);
                        continue;
                    }
                    if write!(out, "{block}").is_err() {
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

/// Rewrite a log line's leading UTC timestamp into local time (ADR-0082).
///
/// The file stays UTC — the rotation boundary is UTC and cannot be moved without
/// taking over the appender, so a file whose contents were local would have a name
/// that disagreed with its contents for eight hours a day in CST. Converting at
/// the moment of display keeps one source of truth and shows the reader a time
/// they can compare with their own clock.
///
/// `+08:00` is deliberately part of the output: a local wall-clock time with no
/// offset is ambiguous the moment the log is pasted into a bug report or crosses a
/// DST change.
///
/// Anything that is not a leading RFC 3339 timestamp is returned untouched, so a
/// separator line or a continuation stays readable — and a timestamp that fails to
/// parse is never dropped, only left as it was.
fn local_time(line: &str) -> Cow<'_, str> {
    local_time_in(line, &chrono::Local)
}

/// [`local_time`] with the zone supplied.
///
/// Split out so the conversion can be tested against a zone the test picks, rather
/// than whichever one the machine happens to run in. A test that asserts
/// `+08:00` because the developer sits in CST proves nothing about the code and
/// fails on CI; one that asserts against an explicit zone proves the offset comes
/// from the argument and is not baked into the function.
fn local_time_in<'a, Tz: chrono::TimeZone>(line: &'a str, tz: &Tz) -> Cow<'a, str>
where
    Tz::Offset: std::fmt::Display,
{
    let Some((stamp, rest)) = line.split_once(' ') else {
        return Cow::Borrowed(line);
    };
    let Ok(utc) = chrono::DateTime::parse_from_rfc3339(stamp) else {
        return Cow::Borrowed(line);
    };
    let local = utc
        .with_timezone(tz)
        .to_rfc3339_opts(chrono::SecondsFormat::AutoSi, false);
    Cow::Owned(format!("{local} {rest}"))
}

/// Render a block of log lines: filter each one, convert its timestamp, keep the
/// newlines.
///
/// The follower hands over a ready-to-print block, so the newlines are part of the
/// contract: dropping one would join two events on one line. Filtering happens here
/// rather than before so `--grep` applies to the tail and to `--follow` through the
/// same code — the version that had two paths is the version where one of them
/// silently stopped filtering.
fn rendered_block(text: &str, filter: &Filter) -> String {
    let mut out = String::with_capacity(text.len() + 16);
    for line in text.split_inclusive('\n') {
        let (body, newline) = match line.strip_suffix('\n') {
            Some(body) => (body, "\n"),
            None => (line, ""),
        };
        if !filter.matches(body) {
            continue;
        }
        out.push_str(&local_time(body));
        out.push_str(newline);
    }
    out
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
        let path = qcode_kernel::logging::current_log(&data_dir);
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
        let current = qcode_kernel::logging::current_log(&self.data_dir);
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

    /// The conversion has to preserve the instant and say which offset it used.
    ///
    /// Asserted against the parsed value, not against a fixed string, so the test
    /// does not depend on which timezone runs it — the failure mode a host-clock
    /// test would have.
    #[test]
    fn a_utc_timestamp_is_shown_in_local_time() {
        let line = "2026-11-12T23:03:37.428189Z  INFO qc::lifecycle: boom";
        let shown = local_time(line);
        let (stamp, rest) = shown.split_once(' ').expect("timestamp and message");
        let parsed = chrono::DateTime::parse_from_rfc3339(stamp).expect("still RFC 3339");
        assert_eq!(
            parsed.to_utc(),
            "2026-11-12T23:03:37.428189Z"
                .parse::<chrono::DateTime<chrono::Utc>>()
                .unwrap(),
            "the instant must not move, only its presentation: {shown}"
        );
        // The invariant that makes the output portable: the offset shown is the
        // one the system reports *for that instant*. A hardcoded `+08:00` fails
        // this on any machine set to another zone.
        use chrono::TimeZone as _;
        assert_eq!(
            parsed.offset(),
            &chrono::Local.offset_from_utc_datetime(&parsed.to_utc().naive_utc()),
            "the offset must come from the system clock, not from a constant: {shown}"
        );
        assert!(
            stamp.contains('+') || stamp.contains("-0") || stamp.ends_with('Z'),
            "the offset must be explicit so the line is unambiguous elsewhere: {shown}"
        );
        // Two spaces separate the timestamp from the level in the rendered line,
        // so `rest` legitimately starts with one; what matters is that the message
        // itself is byte-identical.
        assert_eq!(
            rest, " INFO qc::lifecycle: boom",
            "message must survive untouched"
        );
    }

    /// The offset must be looked up per line, not decided once at startup: a DST
    /// zone shifts by an hour between two lines of the same file. This is why
    /// `chrono::Local` is used at format time instead of a fixed offset captured
    /// when the process began — and why `OffsetTime` was rejected for this job.
    ///
    /// A zone is supplied rather than assumed so the assertion holds on any host.
    #[test]
    fn the_offset_is_looked_up_per_line_not_fixed_at_startup() {
        // `FixedOffset` cannot express DST, so the two offsets are given directly;
        // what is under test is that the value comes from the supplied zone.
        let est = chrono::FixedOffset::west_opt(5 * 3600).unwrap();
        let edt = chrono::FixedOffset::west_opt(4 * 3600).unwrap();
        let winter = local_time_in("2026-01-15T12:00:00.000000Z  INFO qc: winter", &est);
        let summer = local_time_in("2026-07-15T12:00:00.000000Z  INFO qc: summer", &edt);
        assert!(
            winter.starts_with("2026-01-15T07:00:00-05:00"),
            "winter: {winter}"
        );
        assert!(
            summer.starts_with("2026-07-15T08:00:00-04:00"),
            "summer: {summer}"
        );
    }

    /// A non-whole-hour zone must not be rounded: `+05:30` is a real offset and a
    /// `%:z`-less formatter would print it as `+05:00` and be wrong by 30 minutes.
    #[test]
    fn a_half_hour_offset_survives() {
        let ist = chrono::FixedOffset::east_opt(5 * 3600 + 1800).unwrap();
        let shown = local_time_in("2026-11-12T23:03:37.428189Z  INFO qc: probe", &ist);
        assert!(
            shown.starts_with("2026-11-13T04:33:37.428189+05:30"),
            "expected a 30-minute component: {shown}"
        );
    }

    /// A line that is not a timestamped event is left exactly as it was —
    /// separators and continuation lines must not be mangled.
    #[test]
    fn a_line_without_a_timestamp_is_untouched() {
        for line in [
            "--- rotated to qc.2026-11-13.log ---",
            "",
            "no leading time here",
        ] {
            assert_eq!(local_time(line), line);
        }
    }

    /// Newlines are part of the contract with the follower: dropping one would
    /// glue two events together on a single line.
    #[test]
    fn every_line_is_converted_and_newlines_are_kept() {
        let text = "2026-11-12T23:03:37.428189Z  INFO a: one\n\
                    2026-11-12T23:03:38.000000Z  WARN b: two\n";
        let out = rendered_block(text, &no_filter());
        assert_eq!(out.lines().count(), 2);
        assert!(out.ends_with('\n'), "the trailing newline must survive");
        assert!(out.contains("INFO a: one"));
        assert!(out.contains("WARN b: two"));
    }

    /// A data dir with one dated log, so `current_log` finds it without a symlink
    /// (which is how the tests stay platform-neutral — Windows refuses to make one).
    fn fixture(dated: &str) -> (tempfile::TempDir, PathBuf) {
        let dir = tempfile::tempdir().unwrap();
        let logs = qcode_kernel::logging::log_dir(dir.path());
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

        let next = qcode_kernel::logging::log_dir(dir.path()).join("qc.2026-01-02.log");
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
        std::fs::create_dir_all(qcode_kernel::logging::log_dir(dir.path())).unwrap();
        let mut follower = Follower::new(dir.path().to_path_buf());

        assert_eq!(follower.poll().unwrap(), Update::default());
        assert_eq!(follower.poll().unwrap(), Update::default());

        let path = qcode_kernel::logging::log_dir(dir.path()).join("qc.2026-01-01.log");
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

    /// A filter that passes everything.
    fn no_filter() -> Filter {
        Filter::new(None, None).unwrap()
    }

    /// The feature this was added for: the lifecycle lines are two among a hundred
    /// `gpui_wgpu` lines, and `-n 5` showed only the GPU noise.
    #[test]
    fn grep_keeps_the_lines_that_answer_how_the_run_ended() {
        let filter = Filter::new(Some("session (starting|ending)"), None).unwrap();
        let text = "2026-11-13T00:00:01Z  INFO gpui_wgpu::wgpu_context: Testing adapter\n\
                    2026-11-13T00:00:02Z  INFO qc::lifecycle: qc session starting pid=7\n\
                    2026-11-13T00:00:03Z  INFO zed_xim::x11rb: XConnected server\n\
                    2026-11-13T00:00:04Z  INFO qc::lifecycle: qc session ending pid=7 reason=normal\n";
        let out = rendered_block(text, &filter);
        assert_eq!(out.lines().count(), 2, "{out}");
        assert!(out.contains("qc session starting"));
        assert!(out.contains("qc session ending"));
        assert!(!out.contains("wgpu"));
    }

    /// A bad pattern is an error, not a filter that quietly matches everything:
    /// silently showing the wrong lines is worse than refusing.
    #[test]
    fn a_bad_grep_is_reported_instead_of_ignored() {
        let error = match Filter::new(Some("session ("), None) {
            Ok(_) => panic!("an unclosed group has to be refused"),
            Err(error) => error.to_string(),
        };
        assert!(error.contains("--grep"), "must name the flag: {error}");
    }

    /// `--since` in both forms, and the two properties that matter: a duration is
    /// relative to now, and an exact timestamp is taken as written.
    #[test]
    fn since_accepts_a_duration_or_an_exact_time() {
        let now = chrono::Local::now().fixed_offset();
        let half_hour = parse_since("30m").unwrap();
        assert!(
            (now - half_hour - chrono::Duration::minutes(30))
                .num_seconds()
                .abs()
                < 5,
            "30m must mean thirty minutes back"
        );
        assert!(
            (now - parse_since("2h").unwrap() - chrono::Duration::hours(2))
                .num_seconds()
                .abs()
                < 5
        );
        // Parsed and compared, not an epoch literal: the first version of this test
        // hardcoded one and had the number of days wrong.
        let exact = parse_since("2026-11-13T01:02:03Z").unwrap();
        let expected = "2026-11-13T01:02:03Z"
            .parse::<chrono::DateTime<chrono::Utc>>()
            .unwrap();
        assert_eq!(exact.to_utc(), expected);
    }

    /// Labels that are not units are refused, and the message says what is allowed.
    #[test]
    fn since_rejects_a_unit_it_does_not_know() {
        let error = parse_since("5y").unwrap_err().to_string();
        assert!(
            error.contains('y') && error.contains("s, m, h, d"),
            "{error}"
        );
        let error = parse_since("yesterday").unwrap_err().to_string();
        assert!(error.contains("neither a duration"), "{error}");
    }

    /// A number too big for the arithmetic is an error, not a crash.
    ///
    /// Found by hand: `--since 9223372036854775807d` panicked with `attempt to
    /// multiply with overflow` (debug) and, worse, wrapped to a nonsense instant in
    /// a release build — where a panic would at least have been honest.
    #[test]
    fn an_absurd_since_is_refused_instead_of_overflowing() {
        for value in [
            "9223372036854775807d",
            "9223372036854775807w",
            "9223372036854775807h",
        ] {
            let error = parse_since(value).unwrap_err().to_string();
            assert!(
                error.contains("too large") || error.contains("too far back"),
                "{value} has to be refused, not wrapped: {error}"
            );
        }
        // The boundary that must still work: a year back is a real question.
        assert!(parse_since("365d").is_ok());
    }

    /// A line the filter cannot place in time is dropped under `--since`, but the
    /// separator lines are the command talking and have to stay readable.
    #[test]
    fn since_drops_undated_lines_but_keeps_the_separators() {
        let filter = Filter::new(None, Some("1h")).unwrap();
        // Built from `now`, not written as a fixed date: a hardcoded "recent"
        // timestamp is correct today and silently wrong tomorrow, which is exactly
        // how the first version of this test failed.
        let now = chrono::Utc::now().to_rfc3339_opts(chrono::SecondsFormat::Secs, true);
        assert!(filter.matches(&format!("{now}  INFO qc: now")), "recent");
        assert!(
            !Filter::new(None, Some("1h"))
                .unwrap()
                .matches("2020-01-01T00:00:00Z  INFO qc: ancient"),
            "an old line has to be dropped"
        );
        assert!(
            filter.matches("--- following qc.2026-11-13.log ---"),
            "the follower's own separators are not log events"
        );
        assert!(
            !filter.matches("a line with no timestamp"),
            "undated lines cannot satisfy a time bound"
        );
    }

    /// The exact wrong answer that shipped: a search over the tail of one file.
    ///
    /// Reported from a real investigation — `qc logs --grep 'pid=14275'` printed only
    /// `session ending`, and the session looked like it had never started. Rotation is
    /// daily at UTC midnight (which is 08:00 in CST), so a session that began at 07:52
    /// and was killed at 08:00 had its `starting` line in the previous day's file while
    /// `qc logs` printed "older files in …: 3" and searched none of them.
    #[test]
    fn a_search_reaches_back_past_the_rotation() {
        let dir = tempfile::tempdir().unwrap();
        let logs = qcode_kernel::logging::log_dir(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        let yesterday = logs.join("qc.2026-11-15.log");
        let today = logs.join("qc.2026-11-16.log");
        std::fs::write(
            &yesterday,
            "2026-11-15T23:52:23.000000Z  INFO qc::lifecycle: qc session starting pid=14275 \
             version=\"0.12.1\" command=gui\n",
        )
        .unwrap();
        std::fs::write(
            &today,
            "2026-11-16T00:00:56.000000Z  INFO qc::lifecycle: qc session ending pid=14275 \
             exit=143 reason=sigterm uptime_ms=473201\n",
        )
        .unwrap();
        // `log_files` orders by modification time, and both writes above can land in the
        // same clock tick; make yesterday genuinely older.
        std::fs::File::options()
            .write(true)
            .open(&yesterday)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();

        let filter = Filter::new(Some("pid=14275"), None).unwrap();
        let files = qcode_kernel::logging::log_files(dir.path()).unwrap();
        assert_eq!(files.len(), 2);
        let found = filter.tail_across(&files, &today, 40).unwrap();
        assert_eq!(
            found.len(),
            2,
            "both halves of the session have to be found, not just the tail: {found:?}"
        );
        assert!(found[0].contains("session starting"), "{}", found[0]);
        assert!(found[1].contains("session ending"), "{}", found[1]);
    }

    /// `-n` is a total across files, and the *last* matches are the ones kept — the
    /// interesting ones are the most recent, whichever file they are in.
    #[test]
    fn the_line_limit_counts_across_files_and_keeps_the_newest() {
        let dir = tempfile::tempdir().unwrap();
        let logs = qcode_kernel::logging::log_dir(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        let first = logs.join("qc.2026-11-15.log");
        let second = logs.join("qc.2026-11-16.log");
        std::fs::write(&first, "hit one\nhit two\n").unwrap();
        std::fs::write(&second, "hit three\nmiss\n").unwrap();
        std::fs::File::options()
            .write(true)
            .open(&first)
            .unwrap()
            .set_modified(std::time::SystemTime::UNIX_EPOCH)
            .unwrap();

        let filter = Filter::new(Some("hit"), None).unwrap();
        let files = qcode_kernel::logging::log_files(dir.path()).unwrap();
        assert_eq!(
            filter.tail_across(&files, &second, 2).unwrap(),
            vec!["hit two".to_string(), "hit three".to_string()],
            "two lines total, and they must be the newest two"
        );
        assert!(
            filter.tail_across(&files, &second, 0).unwrap().is_empty(),
            "-n 0 shows nothing, even with a filter"
        );
    }

    /// A filter that asks no question leaves the tail alone: no filter means "show me
    /// the end of today's file", and `is_active` is what keeps the two paths apart.
    #[test]
    fn an_empty_filter_is_not_an_across_files_search() {
        assert!(!no_filter().is_active());
        assert!(Filter::new(Some("x"), None).unwrap().is_active());
        assert!(Filter::new(None, Some("1h")).unwrap().is_active());
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
