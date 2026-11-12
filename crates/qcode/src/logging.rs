//! Diagnostics: stderr for the terminal, a rotated file for everything else.
//!
//! The JSONL event stream on stdout is a machine interface and must stay clean,
//! so nothing here ever writes to stdout. See ADR-0077 for why there is a file at
//! all and why it is not log4rs.
//!
//! Two layers, different needs:
//!
//! | layer | target | ANSI | why |
//! |---|---|---|---|
//! | stderr | the terminal | **on** | colour is what makes it readable at a glance |
//! | file | `<data>/logs/qc.log` | **off** | escapes in a file are noise for `grep`/`less` |
//!
//! Both carry a timestamp and the emitting module. The first version of this
//! module turned both off (`without_time`, `with_target(false)`) and left ANSI on,
//! which is exactly backwards for a file: no time to correlate, no module to blame,
//! and every line wrapped in `\x1b[33m`. A captured log looked like
//! `[33m WARN[0m cannot open the MCP log file`.

use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing_appender::rolling::{RollingFileAppender, RollingWriter, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The stable name inside the log directory: a symlink to the current day's file.
///
/// `qc logs` and the user both look for this one name, so nobody has to know the
/// rotation scheme or the date format to find today's log.
pub const CURRENT_LOG: &str = "qc.log";

/// Daily files to keep, current one included.
const KEEP_FILES: usize = 7;

/// Level when neither `RUST_LOG` nor `[log] level` says otherwise.
const DEFAULT_LEVEL: &str = "warn";

/// ANSI for the terminal: on, because colour is what makes a log readable at a
/// glance. Named so [`init`] and its test cannot disagree about the value.
const ANSI_STDERR: bool = true;

/// ANSI for the file: off. A file is read by `grep` and `less`, where escape
/// sequences are noise — the concrete bug was every line wrapped in `[33m`.
const ANSI_FILE: bool = false;

static APPENDER: OnceLock<RollingFileAppender> = OnceLock::new();

/// Zero-sized [`MakeWriter`] that forwards to [`APPENDER`] once it is installed.
///
/// The file cannot be opened during [`init`] because its directory is not known
/// until the CLI has been parsed, and this module must not be the reason a
/// directory appears. Deferring the appender separates the two: `init` puts the
/// stderr layer up with everything that runs before that, and [`attach_file`]
/// fills the file layer in afterwards. Writes that arrive in between are dropped,
/// which is the honest outcome — they precede the moment we knew where to put
/// them, and they are still on stderr.
#[derive(Clone, Copy, Debug)]
struct LogFile;

/// Where a [`LogFile`] write goes: the daily file, or nowhere yet.
enum FileTarget<'a> {
    /// [`attach_file`] has not run, or it failed. Report the bytes as written:
    /// the caller is `tracing`, which cannot act on a short write anyway.
    Nowhere,
    Daily(RollingWriter<'a>),
}

impl io::Write for FileTarget<'_> {
    fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
        match self {
            Self::Nowhere => Ok(buf.len()),
            Self::Daily(writer) => writer.write(buf),
        }
    }

    fn flush(&mut self) -> io::Result<()> {
        match self {
            Self::Nowhere => Ok(()),
            Self::Daily(writer) => writer.flush(),
        }
    }
}

impl<'a> MakeWriter<'a> for LogFile {
    type Writer = FileTarget<'a>;

    fn make_writer(&'a self) -> Self::Writer {
        match APPENDER.get() {
            Some(appender) => FileTarget::Daily(appender.make_writer()),
            None => FileTarget::Nowhere,
        }
    }
}

/// Install the stderr layer and an empty file layer.
///
/// `configured_level` is the `[log] level` value, if the config could be read.
/// `RUST_LOG` wins over it, so the existing escape hatch keeps working.
///
/// Call this after `Cli::parse` (the level comes from the config file it names)
/// and before anything that might want to log.
pub fn init(configured_level: Option<&str>) {
    let filter = filter_for(&level_spec(configured_level));

    let stderr_layer = tracing_subscriber::fmt::layer()
        .with_ansi(ANSI_STDERR)
        .with_target(true)
        .with_writer(io::stderr)
        .with_filter(filter.clone());

    let file_layer = tracing_subscriber::fmt::layer()
        .with_ansi(ANSI_FILE)
        .with_target(true)
        .with_writer(LogFile)
        .with_filter(filter);

    // `try_init` and not `init`: a second call (a re-exec, a test harness that
    // already installed a subscriber) must not panic the application.
    let _ = tracing_subscriber::registry()
        .with(stderr_layer)
        .with(file_layer)
        .try_init();
}

/// Where logs live for a given data directory.
///
/// One function so the three callers that used to spell out
/// `data_dir.join("logs")` cannot drift: the MCP server logs, the network audit
/// trail and `qc.log` all sit here. The definition is in `qcode-core::paths`
/// because `qcode-mcp` cannot import this crate or `qcode-kernel`.
pub fn log_dir(data_dir: &Path) -> PathBuf {
    qcode_core::paths::log_dir(data_dir)
}

/// Start writing to the daily file under `data_dir`, and return its stable path.
///
/// Idempotent, and tolerant of failure: file logging is a diagnostic aid, so a
/// read-only or full disk must not stop the agent from running.
///
/// Failures are recorded at `debug`, not on stderr: the data directory is chosen by
/// `qcode_kernel::data_dir_hint`, so a sandboxed `qc skills list` can reach a
/// directory it may not write, and a command that never uses the store should not
/// print an error about a log file it was never going to fill. `qc logs` reports
/// the absence instead, which is where a user looks for it.
pub fn attach_file(data_dir: &Path) -> PathBuf {
    let dir = log_dir(data_dir);
    let path = dir.join(CURRENT_LOG);

    if APPENDER.get().is_some() {
        return path;
    }
    if let Err(error) = std::fs::create_dir_all(&dir) {
        tracing::debug!(
            path = %dir.display(),
            %error,
            "cannot create the log directory; file logging stays off"
        );
        return path;
    }

    let builder = || {
        RollingFileAppender::builder()
            .rotation(Rotation::DAILY)
            .filename_prefix("qc")
            .filename_suffix("log")
            .max_log_files(KEEP_FILES)
    };

    // The symlink is what `qc logs` and a support request rely on, but creating
    // one needs a privilege Windows does not grant by default. Losing the whole
    // appender over a convenience link would be the wrong trade, so retry
    // without it.
    let built = builder()
        .latest_symlink(CURRENT_LOG)
        .build(&dir)
        .or_else(|error| {
            tracing::debug!(
                %error,
                "cannot create the {CURRENT_LOG} symlink; logging to dated files only"
            );
            builder().build(&dir)
        });

    match built {
        Ok(appender) => {
            let _ = APPENDER.set(appender);
        }
        Err(error) => {
            tracing::debug!(
                path = %dir.display(),
                %error,
                "cannot start file logging; diagnostics go to stderr only"
            );
        }
    }
    path
}

/// Existing log files, oldest first, so a caller can show or tail the newest.
///
/// Sorted by modification time rather than by parsed date: the appender's date
/// format is its business, and a file written at 23:59:59 belongs to yesterday
/// even though it is still the most recent one.
pub fn log_files(data_dir: &Path) -> Vec<PathBuf> {
    let dir = log_dir(data_dir);
    let Ok(entries) = std::fs::read_dir(&dir) else {
        return Vec::new();
    };
    let mut files: Vec<(std::time::SystemTime, PathBuf)> = entries
        .filter_map(Result::ok)
        .filter(|entry| {
            entry.file_type().is_ok_and(|kind| kind.is_file())
                && entry.file_name().to_string_lossy().starts_with("qc.")
                && entry.file_name().to_string_lossy().ends_with(".log")
        })
        .filter_map(|entry| {
            let modified = entry.metadata().ok()?.modified().ok()?;
            Some((modified, entry.path()))
        })
        .collect();
    files.sort();
    files.into_iter().map(|(_, path)| path).collect()
}

/// The file `qc logs` should show when it is not told otherwise.
///
/// Prefers the stable symlink, falls back to the newest dated file (the symlink
/// is absent on platforms where creating one is refused), and finally to the
/// path the next write would create so the message can still name it.
pub fn current_log(data_dir: &Path) -> PathBuf {
    let link = log_dir(data_dir).join(CURRENT_LOG);
    if link.exists() {
        return link;
    }
    log_files(data_dir)
        .pop()
        .unwrap_or_else(|| log_dir(data_dir).join(CURRENT_LOG))
}

/// `RUST_LOG` > `[log] level` > [`DEFAULT_LEVEL`].
fn level_spec(configured_level: Option<&str>) -> String {
    choose_level(std::env::var("RUST_LOG").ok().as_deref(), configured_level)
}

/// The precedence rule itself, with the environment passed in.
///
/// Split out so it can be tested without mutating `RUST_LOG`: the first version of
/// these tests did, and the two that touched it raced each other under the
/// default parallel harness — one would set the variable while the other was
/// reading it, so a passing run proved nothing and a failing one was a coin flip.
fn choose_level(rust_log: Option<&str>, configured_level: Option<&str>) -> String {
    if let Some(from_env) = rust_log.map(str::trim).filter(|v| !v.is_empty()) {
        return from_env.to_string();
    }
    configured_level
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .unwrap_or(DEFAULT_LEVEL)
        .to_string()
}

/// Build the filter, reporting—but surviving—a malformed spec.
fn filter_for(spec: &str) -> EnvFilter {
    // A typo in the log level is not worth failing startup over, but it must not
    // pass silently either: say so, then use the default.
    EnvFilter::try_new(spec).unwrap_or_else(|error| {
        eprintln!(
            "qcode: [log] level {spec:?} is not a valid filter ({error}); using {DEFAULT_LEVEL}"
        );
        EnvFilter::new(DEFAULT_LEVEL)
    })
}

/// Masks known secret values in any string that is about to be logged or shown.
///
/// This is defense in depth: the code never logs credentials on purpose, but
/// provider errors and future debug output can carry arbitrary response bodies.
///
/// Still applied by hand at the two call sites that need it. Turning it into a
/// `tracing` layer — so it cannot be forgotten — is ADR-0077's follow-up, kept
/// separate because it is a security change rather than a plumbing one.
#[derive(Debug, Clone, Default)]
pub struct Redactor {
    secrets: Vec<String>,
}

impl Redactor {
    pub fn new(secrets: impl IntoIterator<Item = String>) -> Self {
        let mut secrets: Vec<String> = secrets
            .into_iter()
            .filter(|secret| secret.len() >= 4)
            .collect();
        // Longest first so overlapping secrets are fully masked.
        secrets.sort_by_key(|secret| std::cmp::Reverse(secret.len()));
        Self { secrets }
    }

    pub fn redact(&self, input: &str) -> String {
        let mut out = input.to_string();
        for secret in &self.secrets {
            out = out.replace(secret.as_str(), "<redacted>");
        }
        out
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::{Arc, Mutex};

    /// Order matters and is easy to get backwards: an explicit `RUST_LOG` must
    /// beat the config file, because that is the debugging escape hatch people
    /// reach for without editing config.
    ///
    /// No environment variable is touched: see [`choose_level`].
    #[test]
    fn rust_log_outranks_the_config_file() {
        assert_eq!(choose_level(Some("debug"), Some("error")), "debug");
        assert_eq!(choose_level(None, Some("error")), "error");
        assert_eq!(choose_level(Some("debug"), None), "debug");
    }

    #[test]
    fn an_empty_setting_falls_back_to_the_default() {
        assert_eq!(choose_level(None, None), DEFAULT_LEVEL);
        assert_eq!(choose_level(None, Some("   ")), DEFAULT_LEVEL);
        assert_eq!(choose_level(Some(""), Some("")), DEFAULT_LEVEL);
    }

    /// A typo must not take the process down, and must not silently change the
    /// level either.
    #[test]
    fn an_unparsable_level_falls_back_instead_of_panicking() {
        // `EnvFilter` accepts `foo` as a target-only directive, so use a string it
        // genuinely rejects rather than one that merely looks odd.
        let filter = filter_for("warn=notalevel");
        assert_eq!(filter.to_string(), DEFAULT_LEVEL);
    }

    /// The path `qc logs` prints and the path the appender writes have to be the
    /// same file, or support requests point at nothing.
    #[test]
    fn current_log_falls_back_to_the_expected_path() {
        let dir = tempfile::tempdir().unwrap();
        let data = dir.path();
        assert_eq!(current_log(data), log_dir(data).join(CURRENT_LOG));

        std::fs::create_dir_all(log_dir(data)).unwrap();
        let older = log_dir(data).join("qc.2026-01-01.log");
        let newer = log_dir(data).join("qc.2026-01-02.log");
        std::fs::write(&older, "a").unwrap();
        std::fs::write(&newer, "b").unwrap();
        // No symlink here, so the newest dated file is the answer.
        assert_eq!(current_log(data), newer);
        assert_eq!(log_files(data), vec![older, newer]);
    }

    /// Files that are not ours must not be picked up: the same directory holds
    /// `mcp-*.log` and `network.jsonl`.
    #[test]
    fn log_files_ignores_neighbours() {
        let dir = tempfile::tempdir().unwrap();
        let logs = log_dir(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("mcp-wordcraft.log"), "x").unwrap();
        std::fs::write(logs.join("network.jsonl"), "x").unwrap();
        std::fs::write(logs.join("qc.2026-01-01.log"), "x").unwrap();
        assert_eq!(log_files(dir.path()).len(), 1);
    }

    /// The reason the file layer exists: a captured log has to be greppable.
    ///
    /// Two halves, because either alone would be a tautology: that [`init`] uses
    /// [`ANSI_FILE`] for the file layer, and that a layer built with it really
    /// emits no escape bytes. The bug this guards against is concrete — a
    /// captured log read `[33m WARN[0m cannot open the MCP log file`.
    #[test]
    fn the_file_layer_emits_no_ansi_escapes() {
        // Both sides render through the same helper, fed the very constants
        // `init` passes. The terminal half is not decoration: without it a
        // silently broken helper would make the file half pass for the wrong
        // reason.
        let terminal = rendered_line(ANSI_STDERR);
        assert!(
            terminal.contains('\u{1b}'),
            "stderr keeps colour, and this proves the helper can emit escapes at all"
        );

        assert!(
            !rendered_line(ANSI_FILE).contains('\u{1b}'),
            "ANSI_FILE must stay off: an escape byte in the log breaks grep"
        );
    }

    /// Render one warning through a `fmt` layer configured like the real ones,
    /// into a buffer instead of a terminal or file.
    fn rendered_line(ansi: bool) -> String {
        #[derive(Clone)]
        struct Sink(Arc<Mutex<Vec<u8>>>);

        struct SinkWriter(Arc<Mutex<Vec<u8>>>);

        impl io::Write for SinkWriter {
            fn write(&mut self, buf: &[u8]) -> io::Result<usize> {
                self.0.lock().unwrap().extend_from_slice(buf);
                Ok(buf.len())
            }
            fn flush(&mut self) -> io::Result<()> {
                Ok(())
            }
        }

        impl<'a> MakeWriter<'a> for Sink {
            type Writer = SinkWriter;
            fn make_writer(&'a self) -> Self::Writer {
                SinkWriter(self.0.clone())
            }
        }

        let buffer = Arc::new(Mutex::new(Vec::new()));
        let subscriber = tracing_subscriber::fmt()
            .with_ansi(ansi)
            .with_target(true)
            .with_writer(Sink(buffer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, || {
            tracing::warn!("boom");
        });
        let bytes = buffer.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}
