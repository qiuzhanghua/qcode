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

use std::fmt;
use std::io;
use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use tracing::{Event, Subscriber};
use tracing_appender::rolling::{RollingFileAppender, RollingWriter, Rotation};
use tracing_subscriber::EnvFilter;
use tracing_subscriber::Layer as _;
use tracing_subscriber::fmt::FmtContext;
use tracing_subscriber::fmt::format::{Format, FormatEvent, FormatFields, Full, Writer};
use tracing_subscriber::fmt::time::SystemTime;
use tracing_subscriber::fmt::writer::MakeWriter;
use tracing_subscriber::layer::SubscriberExt as _;
use tracing_subscriber::registry::LookupSpan;
use tracing_subscriber::util::SubscriberInitExt as _;

/// The stable name inside the log directory: a symlink to the current day's file.
///
/// `qc logs` and the user both look for this one name, so nobody has to know the
/// rotation scheme or the date format to find today's log.
pub const CURRENT_LOG: &str = "qc.log";

/// Daily files to keep, current one included.
const KEEP_FILES: usize = 7;

/// Level when nothing says otherwise, for the file: **our** crates.
///
/// `info` and not `warn`: the file is the only record of a run for a desktop
/// launch (there is no terminal to scroll back), and a log that holds nothing but
/// warnings cannot answer "what did it actually do". See ADR-0082.
const DEFAULT_LEVEL_FILE_OWN: tracing::Level = tracing::Level::INFO;

/// Level when nothing says otherwise, for the file: everything else.
///
/// A GUI start logs ~100 lines of `gpui_wgpu` adapter probing, `zed_xim` X11
/// handshakes and `gpui_linux` frame timing. At `info` for all of it, the two
/// lifecycle lines that say how the run ended are a rounding error in the noise —
/// measured: `qc logs -n 5` showed nothing but GPU setup. Those crates are still
/// available at `warn` (their failures are ours to see), and `RUST_LOG` still
/// turns them up when the window itself is what is being debugged.
const DEFAULT_LEVEL_FILE_OTHER: tracing::Level = tracing::Level::WARN;

/// Level when nothing says otherwise, for the terminal.
///
/// `warn` while the file sits at `info` is the point of the split: an interactive
/// `qc run` must not narrate every session it creates over the user's prompt, but
/// the same run should still leave a usable record behind.
const DEFAULT_LEVEL_STDERR: &str = "warn";

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
    // Validated once, here, and not per destination.
    //
    // The fallback for an unparsable spec has to be *the defaults*, and the file's
    // default is not a spec string at all (it is the target rule below). Letting
    // each destination fall back on its own meant a typo made the file use stderr's
    // `warn` — quieter than saying nothing, and silent about it.
    let explicit = explicit_level(std::env::var("RUST_LOG").ok().as_deref(), configured_level)
        .and_then(usable_spec);
    let stderr_spec = explicit.as_deref().unwrap_or(DEFAULT_LEVEL_STDERR);

    // Built inside each branch: a `Layer` is not `Clone`, and the two branches
    // need differently typed file filters.
    let stderr_layer = || {
        tracing_subscriber::fmt::layer()
            .event_format(RedactedFormat::new(ANSI_STDERR))
            .with_writer(io::stderr)
            .with_filter(filter_for(stderr_spec, DEFAULT_LEVEL_STDERR))
    };
    let file_layer = || {
        tracing_subscriber::fmt::layer()
            .event_format(RedactedFormat::new(ANSI_FILE))
            .with_writer(LogFile)
    };

    // `try_init` and not `init`: a second call (a re-exec, a test harness that
    // already installed a subscriber) must not panic the application.
    let _ = match &explicit {
        // An explicit ask is one filter for both destinations: `RUST_LOG=debug`
        // means "show me everything", not "show me our crates and nothing else".
        Some(spec) => tracing_subscriber::registry()
            .with(stderr_layer())
            .with(file_layer().with_filter(filter_for(spec, DEFAULT_LEVEL_STDERR)))
            .try_init(),
        // Nobody said anything: the file is a record of what *we* did, so our
        // crates speak at `info` and the framework chatter stays at `warn`.
        None => tracing_subscriber::registry()
            .with(stderr_layer())
            .with(file_layer().with_filter(OwnCratesAtInfo))
            .try_init(),
    };
}

/// A spec that `EnvFilter` accepts as written, or nothing plus a warning.
///
/// Two failure modes, and the second is the dangerous one:
///
/// - **It does not parse** (`warn=notalevel`): say so and use the defaults.
/// - **It parses as something else entirely** (`inf`, `warning`, `debugg`): a bare
///   word is a *target* directive to `EnvFilter`, so a misspelled level silently
///   enables a target that matches nothing and switches everything else off. The
///   file comes out empty and there is no error to find — measured: `RUST_LOG=inf`
///   wrote 0 lines where the default writes 2. That is a typo, not a request to
///   silence the program.
///
/// Both are ignored rather than honoured, for the same reason: a desktop launch has
/// no stderr, so a warning written there is read by nobody — and the file it would
/// have explained is the empty one. Falling back to the defaults means a typo costs at
/// most the verbosity that was asked for, never the record. `RUST_LOG=qcode_gui` (a
/// real target) is untouched: no level name is a prefix of it.
fn usable_spec(spec: String) -> Option<String> {
    if let Err(error) = EnvFilter::try_new(&spec) {
        eprintln!(
            "qcode: [log] level {spec:?} is not a valid filter ({error}); using the defaults"
        );
        return None;
    }
    if let Some(level) = misspelled_level(&spec) {
        eprintln!(
            "qcode: [log] level {spec:?} is not a level name; it would only enable a target by \
             that name and log nothing else. Using the defaults — did you mean {level:?}?"
        );
        return None;
    }
    Some(spec)
}

/// The level a bare word was almost certainly meant to be, if any.
///
/// Only for words that are a prefix of a level or a level plus a letter, which is
/// how typos of these six words actually look (`inf`, `warning`, `debugg`).
/// `qcode_gui` and `warn,hyper=off` are not bare words and never reach this.
fn misspelled_level(spec: &str) -> Option<&'static str> {
    const LEVELS: [&str; 6] = ["off", "error", "warn", "info", "debug", "trace"];
    if spec.contains(['=', ',']) {
        return None;
    }
    let word = spec.trim().to_ascii_lowercase();
    if word.len() < 2 || LEVELS.contains(&word.as_str()) {
        return None;
    }
    LEVELS
        .into_iter()
        .find(|level| level.starts_with(&word) || word.starts_with(level))
}

/// Whether an event's target belongs to this workspace.
///
/// Targets are module paths, so the first segment is the crate name with its
/// dashes turned into underscores: `qcode-gui` logs as `qcode_gui::shell::mod`.
/// A prefix match on `qcode` therefore does *not* cover `qcode_gui` — the whole
/// reason this is a rule rather than a list of eighteen names is so that adding a
/// crate cannot silently drop its `info` lines; the alternative needs a drift test
/// against the manifests to stay honest.
///
/// `qc` is in the list because the binary target is named `qc`, not `qcode`: every
/// module compiled into it logs as `qc::logging`, `qc::lifecycle`. Omitting it
/// filters out the lifecycle lines this whole file exists to keep — which is
/// exactly what the first version of this function did, and why the fix is pinned
/// by a test that names the real target instead of a plausible one.
///
/// No other binary needs a name here: `logging::init` is called from `qc` only, so
/// `qc-eval` and the MCP probe servers never install this layer and their targets
/// never reach it.
fn owns_target(target: &str) -> bool {
    let crate_name = target.split("::").next().unwrap_or(target);
    // `qcd` 是 daemon 的 crate。少了它，一个没有终端的常驻进程的所有 `info`
    // 都进不了 `qc.log`——而那是它唯一的现场（ADR-0092 §11 阶段 3）。
    //
    // 实测症状：`qc daemon stop` 之后日志里只有 `lifecycle` 那两行，`qcd` 自己
    // 写的「stop requested over the control channel」一条都没有，「它为什么停了」
    // 于是无从回答。
    crate_name == "qc"
        || crate_name == "qcd"
        || crate_name == "qcode"
        || crate_name.starts_with("qcode_")
}

/// The file's default filter: our crates at `info`, everything else at `warn`.
///
/// A hand-written [`Filter`] rather than `filter_fn`, because only a real impl can
/// report `max_level_hint` — without one every `debug!` in a streaming hot path
/// would be *formatted* before being discarded, and the terminal would pay for a
/// log level nobody asked for.
struct OwnCratesAtInfo;

impl<S: Subscriber> tracing_subscriber::layer::Filter<S> for OwnCratesAtInfo {
    fn enabled(
        &self,
        meta: &tracing::Metadata<'_>,
        _cx: &tracing_subscriber::layer::Context<'_, S>,
    ) -> bool {
        let ceiling = if owns_target(meta.target()) {
            DEFAULT_LEVEL_FILE_OWN
        } else {
            DEFAULT_LEVEL_FILE_OTHER
        };
        *meta.level() <= ceiling
    }

    fn max_level_hint(&self) -> Option<tracing::level_filters::LevelFilter> {
        // The most verbose any event can be. Saying `INFO` here is what keeps
        // `debug!`/`trace!` from being built at all.
        Some(tracing::level_filters::LevelFilter::INFO)
    }
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
pub fn log_files(data_dir: &Path) -> std::io::Result<Vec<PathBuf>> {
    let dir = log_dir(data_dir);
    // The error is handed back rather than flattened to "no files": a directory
    // that cannot be read is not an empty one, and `qc logs` said "no log files in
    // …" for a directory holding 13 KB of them. A user reading that concludes
    // logging is off, which is the one diagnosis that is certainly wrong.
    let entries = std::fs::read_dir(&dir)?;
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
    Ok(files.into_iter().map(|(_, path)| path).collect())
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
        .ok()
        .and_then(|mut files| files.pop())
        .unwrap_or_else(|| log_dir(data_dir).join(CURRENT_LOG))
}

/// `RUST_LOG` > `[log] level`, or nothing.
///
/// One value for both destinations: an explicit level is an ask about *this
/// process*, not about one destination, and `RUST_LOG=debug` must not leave the
/// terminal quiet when the whole reason to type it was to watch the terminal. Only
/// the absent case splits (ADR-0082), and [`init`] is where the split happens.
///
/// Split out so it can be tested without mutating `RUST_LOG`: the first version of
/// these tests did, and the two that touched it raced each other under the
/// default parallel harness — one would set the variable while the other was
/// reading it, so a passing run proved nothing and a failing one was a coin flip.
fn explicit_level(rust_log: Option<&str>, configured_level: Option<&str>) -> Option<String> {
    if let Some(from_env) = rust_log.map(str::trim).filter(|v| !v.is_empty()) {
        return Some(from_env.to_string());
    }
    configured_level
        .map(str::trim)
        .filter(|value| !value.is_empty())
        .map(str::to_string)
}

/// Build the filter, reporting—but surviving—a malformed spec.
fn filter_for(spec: &str, fallback: &str) -> EnvFilter {
    // A typo in the log level is not worth failing startup over, but it must not
    // pass silently either: say so, then use the default.
    EnvFilter::try_new(spec).unwrap_or_else(|error| {
        eprintln!("qcode: [log] level {spec:?} is not a valid filter ({error}); using {fallback}");
        EnvFilter::new(fallback)
    })
}

/// Redacts the fully rendered line, then hands it to the real writer.
///
/// Wrapping the *formatter* rather than the writer is the whole point: `fmt`
/// emits a line in many small `write_str` calls, so a writer-level filter would
/// see a secret split across two of them and miss it. Rendering into a buffer
/// first makes the pattern matching see what the user would see.
///
/// A secret can also arrive through a field rather than the message — `?base_url`
/// on a provider that embeds the key in the URL — which is why the whole line is
/// scanned rather than just `message`.
#[derive(Clone)]
struct RedactedFormat {
    inner: Format<Full, SystemTime>,
}

impl RedactedFormat {
    fn new(ansi: bool) -> Self {
        Self {
            // The layer's own `with_ansi`/`with_target` no longer apply once an
            // event format is supplied, so they are set here. `with_ansi` also
            // overrides the writer's idea of colour, which is what lets the file
            // layer stay escape-free while stderr keeps its colours.
            inner: Format::default().with_ansi(ansi).with_target(true),
        }
    }
}

impl<S, N> FormatEvent<S, N> for RedactedFormat
where
    S: Subscriber + for<'a> LookupSpan<'a>,
    N: for<'a> FormatFields<'a> + 'static,
{
    fn format_event(
        &self,
        ctx: &FmtContext<'_, S, N>,
        mut writer: Writer<'_>,
        event: &Event<'_>,
    ) -> fmt::Result {
        let redactor = crate::redact::current();
        if redactor.is_empty() {
            // Nothing to mask: stream straight through rather than formatting
            // twice. This is the common case for a CLI run with no key resolved.
            return self.inner.format_event(ctx, writer, event);
        }
        let mut line = String::new();
        self.inner
            .format_event(ctx, Writer::new(&mut line), event)?;
        writer.write_str(&redactor.redact(&line))
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
    /// No environment variable is touched: see [`explicit_level`].
    #[test]
    fn rust_log_outranks_the_config_file() {
        assert_eq!(
            explicit_level(Some("debug"), Some("error")).as_deref(),
            Some("debug")
        );
        assert_eq!(
            explicit_level(None, Some("error")).as_deref(),
            Some("error")
        );
        assert_eq!(
            explicit_level(Some("debug"), None).as_deref(),
            Some("debug")
        );
    }

    #[test]
    fn an_empty_setting_counts_as_absent() {
        assert_eq!(explicit_level(None, None), None);
        assert_eq!(explicit_level(None, Some("   ")), None);
        assert_eq!(explicit_level(Some(""), Some("")), None);
    }

    /// The defaults split, and that is the whole point of ADR-0082: the file has
    /// to be a usable record, the terminal has to stay out of the prompt.
    #[test]
    fn absent_setting_splits_the_file_record_from_a_quiet_terminal() {
        assert_eq!(explicit_level(None, None), None);
        assert_eq!(DEFAULT_LEVEL_FILE_OWN, tracing::Level::INFO);
        assert_eq!(DEFAULT_LEVEL_FILE_OTHER, tracing::Level::WARN);
        // The stderr default is a spec string, so it is compared through the filter
        // that actually consumes it rather than by eyeballing the literal.
        assert_eq!(filter_for(DEFAULT_LEVEL_STDERR, "off").to_string(), "warn");
        assert!(
            DEFAULT_LEVEL_FILE_OWN > DEFAULT_LEVEL_FILE_OTHER,
            "our crates must be the verboser of the two, or the split does nothing"
        );
    }

    /// An explicit ask applies to both: `RUST_LOG=debug` exists to watch the
    /// terminal, so clamping stderr to `warn` would make the escape hatch useless.
    ///
    /// Asserted through `explicit_level`, which is what both destinations consult;
    /// the old version of this test went through a helper that also spelled out the
    /// defaults, so it could pass while `init` ignored the value.
    #[test]
    fn an_explicit_level_applies_to_both_destinations() {
        for (rust_log, configured, expected) in [
            (Some("debug"), None, "debug"),
            (None, Some("error"), "error"),
            // `RUST_LOG` wins, and it is the one that reaches both destinations.
            (Some("debug"), Some("error"), "debug"),
        ] {
            assert_eq!(
                explicit_level(rust_log, configured).as_deref(),
                Some(expected)
            );
        }
    }

    /// The rule that replaces a list of eighteen crate names.
    ///
    /// The failure mode being pinned, in the order both were actually hit:
    ///
    /// 1. `qc::lifecycle` is the **binary's** target (the bin is named `qc`, not
    ///    `qcode`), and a rule written only around `qcode*` filtered out the
    ///    lifecycle lines — the file came out empty. Unit tests missed it because
    ///    the list of "own" targets was written from the package names.
    /// 2. `EnvFilter` prefix matching stops at `::`, so `qcode=info` says nothing
    ///    about `qcode_gui` — the GUI, where a desktop run spends all its time,
    ///    would silently fall back to `warn`.
    #[test]
    fn our_own_crates_are_recognised_by_target() {
        for own in [
            // The binary target, as it really appears in the file.
            "qc",
            "qc::lifecycle",
            "qc::logging",
            "qc::commands::logs",
            // The daemon's binary target. It has no terminal, so the file is the
            // only place its `info` can go (ADR-0092 §11 阶段 3).
            "qcd",
            "qcd::detach",
            // The libraries.
            "qcode",
            "qcode_gui",
            "qcode_gui::shell::mod",
            "qcode_kernel::config",
            "qcode_tools::sandbox",
            "qcode_terminal::screen",
        ] {
            assert!(owns_target(own), "{own} is ours");
        }
        for other in [
            "gpui_wgpu::wgpu_context",
            "zed_xim::x11rb",
            "gpui_linux::linux::x11::client",
            "hyper_util::client",
            "xim_ctext",
        ] {
            assert!(!owns_target(other), "{other} is not ours");
        }
        // The boundary a naive `contains("qcode")` would get wrong, in both
        // directions: a prefix that merely starts with the letters is not ours, and
        // a future crate called `qcode_extras` is.
        assert!(!owns_target("not_qcode"), "the name has to start with it");
        assert!(
            !owns_target("qcdx"),
            "`qcd` must be a whole segment, not a prefix of another crate name"
        );
        assert!(
            !owns_target("qcodezip"),
            "`qcode` must be a whole segment, not a prefix of another crate name"
        );
        assert!(
            owns_target("qcode_extras::x"),
            "a crate added later is covered without editing a list"
        );
    }

    /// A typo in the level must not switch logging off, and must not quietly make
    /// the file quieter than saying nothing.
    ///
    /// Two distinct traps, both measured on the real binary before this was fixed:
    ///
    /// - `RUST_LOG=inf` does **not** fail to parse — `EnvFilter` reads a bare word as
    ///   a *target* directive, so it enables a target called `inf` and turns
    ///   everything else off. The file came out with 0 lines where the default writes
    ///   2, and nothing said why.
    /// - With the typo accepted, the per-destination fallback used to hand the file
    ///   stderr's `warn`, so the file lost our `info` lines even in cases where the
    ///   spec did fail to parse.
    #[test]
    fn a_misspelled_level_is_reported_not_silently_taken_as_a_target() {
        // A real target is not a misspelling, whatever it looks like.
        assert_eq!(misspelled_level("qcode_gui"), None);
        assert_eq!(
            misspelled_level("inf=debug"),
            None,
            "a directive, not a word"
        );
        assert_eq!(misspelled_level("warn,qcode=info"), None);
        assert_eq!(
            misspelled_level("info"),
            None,
            "the real word is not a typo"
        );
        assert_eq!(misspelled_level("off"), None);

        // The typos that actually silence the log.
        for (typo, meant) in [
            ("inf", "info"),
            ("warning", "warn"),
            ("debugg", "debug"),
            ("erro", "error"),
            ("trac", "trace"),
        ] {
            assert_eq!(misspelled_level(typo), Some(meant), "{typo}");
            // Ignored, not honoured: honouring it is what blacked the log out.
            assert_eq!(usable_spec(typo.to_string()), None);
        }
    }

    /// The fallback for an unparsable spec is *the defaults*, which for the file
    /// means the target rule — not stderr's `warn`.
    ///
    /// Before this, `usable_spec` did not exist and each destination fell back on its
    /// own: the file fell back to `warn`, so one typo made it quieter than leaving
    /// `RUST_LOG` unset.
    #[test]
    fn an_unparsable_spec_falls_back_to_the_defaults_not_to_warn() {
        assert_eq!(usable_spec("warn=notalevel".to_string()), None);
        // `None` is what `init` reads as "nobody said anything", and that branch
        // installs `OwnCratesAtInfo` for the file and `DEFAULT_LEVEL_STDERR` for the
        // terminal.
        assert_eq!(usable_spec("debug".to_string()).as_deref(), Some("debug"));
    }

    /// The hint is what keeps `debug!` from being formatted in a hot path; without
    /// it every streaming token would be rendered and then thrown away.
    #[test]
    fn the_file_filter_advertises_a_useful_level_hint() {
        // `Filter<S>` is generic, so the impl has to be picked for a subscriber
        // type before the method can be named at all.
        let hint = <OwnCratesAtInfo as tracing_subscriber::layer::Filter<
            tracing_subscriber::Registry,
        >>::max_level_hint(&OwnCratesAtInfo);
        assert_eq!(
            hint,
            Some(tracing::level_filters::LevelFilter::INFO),
            "a missing hint makes the macros construct every debug event"
        );
    }

    /// The default, through the real layer machinery rather than the rule function:
    /// a `gpui_wgpu` info line must not reach the file, but its warning must.
    ///
    /// This is the property the whole change is for. Asserted on what the layer
    /// writes, because `owns_target` being right says nothing about whether the
    /// filter is wired into the file layer at all — the first attempt at this had
    /// the rule correct and still wrote nothing, for `qc::lifecycle` (see
    /// `our_own_crates_are_recognised_by_target`).
    #[test]
    fn the_file_default_keeps_third_party_info_out() {
        #[derive(Clone)]
        struct Sink(Arc<std::sync::Mutex<Vec<u8>>>);

        struct SinkWriter(Arc<std::sync::Mutex<Vec<u8>>>);

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

        let buffer = Arc::new(std::sync::Mutex::new(Vec::new()));
        let layer = tracing_subscriber::fmt::layer()
            .event_format(RedactedFormat::new(ANSI_FILE))
            .with_writer(Sink(buffer.clone()))
            .with_filter(OwnCratesAtInfo);
        let subscriber = tracing_subscriber::registry().with(layer);

        tracing::subscriber::with_default(subscriber, || {
            tracing::event!(target: "qc::lifecycle", tracing::Level::INFO, "ours-info");
            tracing::event!(
                target: "gpui_wgpu::wgpu_context",
                tracing::Level::INFO,
                "theirs-info"
            );
            tracing::event!(
                target: "gpui_wgpu::wgpu_context",
                tracing::Level::WARN,
                "theirs-warn"
            );
            tracing::event!(target: "qc::lifecycle", tracing::Level::DEBUG, "ours-debug");
            // `ERROR` is numerically the *lowest* level, so an inverted comparison
            // would still drop third-party `info` and still pass everything else this
            // test asks for — while silently swallowing every error in the log.
            tracing::event!(
                target: "gpui_wgpu::wgpu_context",
                tracing::Level::ERROR,
                "theirs-error"
            );
        });

        let out = String::from_utf8_lossy(&buffer.lock().unwrap()).into_owned();
        assert!(
            out.contains("ours-info"),
            "our own info must be kept: {out}"
        );
        assert!(
            !out.contains("theirs-info"),
            "the adapter-probing noise is exactly what this default drops: {out}"
        );
        assert!(
            out.contains("theirs-warn"),
            "a third-party warning is still ours to see: {out}"
        );
        assert!(
            out.contains("theirs-error"),
            "an inverted level comparison would look right and eat every error: {out}"
        );
        assert!(
            !out.contains("ours-debug"),
            "the hint caps at info, so debug must not be built: {out}"
        );
    }

    /// A typo must not take the process down, and must not silently change the
    /// level either.
    #[test]
    fn an_unparsable_level_falls_back_instead_of_panicking() {
        // `EnvFilter` accepts `foo` as a target-only directive, so use a string it
        // genuinely rejects rather than one that merely looks odd.
        let filter = filter_for("warn=notalevel", DEFAULT_LEVEL_STDERR);
        assert_eq!(filter.to_string(), DEFAULT_LEVEL_STDERR);
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
        assert_eq!(
            log_files(data).expect("list the log files"),
            vec![older, newer]
        );
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
        assert_eq!(log_files(dir.path()).expect("list the log files").len(), 1);
    }

    /// An unreadable log directory is an error, not an empty one.
    ///
    /// The bug this guards against: `log_files` flattened the `read_dir` failure
    /// into `Vec::new()`, so `qc logs` printed "no log files in <dir>" for a
    /// directory holding 13 KB of them. That is the one message that sends a user
    /// looking for a logging bug — "logging is off" — when the truth is "you
    /// cannot read this".
    #[cfg(unix)]
    #[test]
    fn an_unreadable_log_directory_is_reported_not_emptied() {
        use std::os::unix::fs::PermissionsExt as _;

        let dir = tempfile::tempdir().unwrap();
        let logs = log_dir(dir.path());
        std::fs::create_dir_all(&logs).unwrap();
        std::fs::write(logs.join("qc.2026-01-01.log"), "x").unwrap();

        // Restored before the tempdir is dropped, or cleanup fails on some systems.
        let original = std::fs::metadata(&logs).unwrap().permissions();
        std::fs::set_permissions(&logs, std::fs::Permissions::from_mode(0o000)).unwrap();
        let listed = log_files(dir.path());
        // Asked while it is still unreadable, which is the state `qc logs` is in
        // when it prints the path.
        let named = current_log(dir.path());
        std::fs::set_permissions(&logs, original).unwrap();

        match listed {
            Ok(files) => panic!("an unreadable directory was listed as {files:?}"),
            Err(error) => assert_eq!(
                error.kind(),
                std::io::ErrorKind::PermissionDenied,
                "the error has to name the real cause: {error}"
            ),
        }
        // `current_log` still names a path: `qc logs` prints it before reading, and
        // a directory it cannot list must not leave the message with no file name.
        assert_eq!(named, logs.join(CURRENT_LOG));
    }

    /// The reason the file layer exists: a captured log has to be greppable.
    ///
    /// The bug this guards against is concrete — a captured log read
    /// `[33m WARN[0m cannot open the MCP log file`. The terminal half is not
    /// decoration: without it a silently broken helper would make the file half
    /// pass for the wrong reason.
    #[test]
    fn the_file_layer_emits_no_ansi_escapes() {
        assert!(
            rendered(ANSI_STDERR, || tracing::warn!("boom")).contains('\u{1b}'),
            "stderr keeps colour, and this proves the helper can emit escapes at all"
        );
        assert!(
            !rendered(ANSI_FILE, || tracing::warn!("boom")).contains('\u{1b}'),
            "ANSI_FILE must stay off: an escape byte in the log breaks grep"
        );
    }

    /// The point of the whole exercise: a secret must not reach a log line, whether
    /// it arrives in the message, in a `Display` field, or in a `Debug` field.
    ///
    /// The pure rules of masking live in `qcode_kernel::redact`; what is tested
    /// here is the plumbing — that the formatter every layer uses actually consults
    /// them. Only asserts absence of a secret this test installs itself, so it
    /// cannot race with the others: the set only ever grows, and a set that is too
    /// large redacts too much — never too little.
    #[test]
    fn secrets_never_reach_a_rendered_line() {
        const KEY: &str = "sk-live-THIS-MUST-NEVER-APPEAR";
        crate::redact::install([KEY.to_string()]);

        let line = rendered(ANSI_FILE, || {
            // The same secret twice in one line: a partial mask (first occurrence
            // only) would still leak the second.
            tracing::warn!(api_key = %KEY, "provider rejected the request for {KEY}");
            tracing::warn!(debug = ?Some(KEY), "with debug formatting");
            // A secret inside a URL is the realistic shape: providers that put the
            // key in the query string do exist, and `?base_url` is logged.
            tracing::warn!(base_url = %format!("https://example.test/v1?key={KEY}"), "resolved");
        });

        assert!(!line.contains(KEY), "the key reached the log line: {line}");
        assert!(
            line.matches("<redacted>").count() >= 4,
            "every occurrence should be masked, not just the first: {line}"
        );
    }

    /// Render one warning through a `fmt` layer configured like the real ones,
    /// into a buffer instead of a terminal or file.
    ///
    /// Uses [`RedactedFormat`] rather than a plain format, so every test above
    /// exercises the code path `init` actually installs.
    fn rendered(ansi: bool, emit: impl FnOnce()) -> String {
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
            .event_format(RedactedFormat::new(ansi))
            .with_writer(Sink(buffer.clone()))
            .finish();
        tracing::subscriber::with_default(subscriber, emit);
        let bytes = buffer.lock().unwrap().clone();
        String::from_utf8_lossy(&bytes).into_owned()
    }
}
