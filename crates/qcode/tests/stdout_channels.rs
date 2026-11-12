//! Standard output carries products; narration goes to stderr.
//!
//! `docs/design/LOGGING.md` states the rule ("stdout = 产物, stderr = 叙述") and
//! ADR-0079 migrated the `eprintln!` sites that broke it. That sweep assumed the
//! other direction never happened, and nobody checked — so two real violations
//! survived inside the `qc` binary:
//!
//! - `commands/gui.rs` printed its startup summary to stdout, where a `.desktop`
//!   launch throws it away (exactly when a user wants to know which provider and
//!   how many tools the kernel resolved).
//! - `update.rs` printed `install …? [y/N]` to stdout. With `qc update >
//!   out.log` and stdin still a terminal, the question lands in the file and the
//!   command waits on a silent terminal — it reads as a hang.
//!
//! These tests pin the invariant at the source, because the interesting paths
//! need a `gui` feature, a display, or a live release server, while the rule has
//! to hold in every build.
//!
//! Test modules are out of scope: their printing is a test talking to itself.

use std::collections::BTreeSet;
use std::path::{Path, PathBuf};

/// Files under `crates/qcode/src` whose stdout output is a product someone
/// consumes. A new entry is a decision, not a formality: it has to say what
/// reads it.
const PRODUCT_FILES: &[&str] = &[
    // The JSONL event stream plus the `--config`/`--mcp`/pause reports.
    "output.rs",
    // `qc skills`, `qc shell-env`, `qc sandbox`: the reports themselves.
    "commands/misc.rs",
    // The model's answer, `session:`, `cost:` — human mode of `qc run`.
    "commands/run.rs",
    // `qc config` key/value dumps and MCP edits.
    "commands/settings.rs",
    // The log lines the user asked to see.
    "commands/logs.rs",
    // The `api_key_command` snippet `config key set` tells the user to paste.
    "commands/sessions.rs",
    // The diagnostic checklist.
    "doctor.rs",
    // `current version:`, `latest release:`, `installed …` — a script reads these.
    "update.rs",
    // The Windows probe's report.
    "sandbox_probe.rs",
    // `qc whoami` on Windows (a diagnostic).
    "main.rs",
];

/// What a line does to stdout, if anything.
#[derive(Debug, PartialEq, Eq)]
enum Write {
    /// Nothing, or `eprint!`/`eprintln!`, or a comment.
    None,
    /// `println!` — a line of output.
    Line,
    /// `print!` — a prompt: it leaves the cursor on the same line, which is only
    /// what a question wants.
    Prompt,
}

fn classify(line: &str) -> Write {
    // Prose and commented-out code are allowed to name the macros this file
    // forbids.
    let trimmed = line.trim_start();
    if trimmed.starts_with("//") {
        return Write::None;
    }
    let bytes = line.as_bytes();
    let mut prompt = false;
    let mut from = 0;
    while let Some(found) = line[from..].find("print") {
        let at = from + found;
        let after = at + "print".len();
        // `eprint`/`eprintln` continue with `ln!` or `!` just like `print`; only
        // a preceding `e` tells them apart.
        if at > 0 && bytes[at - 1] == b'e' {
            from = after;
            continue;
        }
        let rest = &line[after..];
        if rest.starts_with("ln!") {
            return Write::Line;
        }
        if rest.starts_with('!') {
            prompt = true;
        }
        from = after;
    }
    if prompt { Write::Prompt } else { Write::None }
}

/// Every `.rs` file under `dir`, skipping test modules.
fn sources(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(current) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&current) else {
            continue;
        };
        for entry in entries {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                // A `tests` directory holds test binaries, not shipped code.
                if path.file_name().is_some_and(|name| name == "tests") {
                    continue;
                }
                stack.push(path);
                continue;
            }
            let is_source = path.extension().and_then(|ext| ext.to_str()) == Some("rs");
            let is_test_module = path
                .file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name == "tests.rs");
            if is_source && !is_test_module {
                found.push(path);
            }
        }
    }
    found.sort();
    found
}

fn qc_src() -> PathBuf {
    Path::new(env!("CARGO_MANIFEST_DIR")).join("src")
}

/// The scanner is the thing under test; prove it can see what it looks for, or
/// the assertions below could pass because it found nothing at all.
#[test]
fn the_classifier_tells_the_streams_apart() {
    assert_eq!(classify(r#"println!("session: {}", id);"#), Write::Line);
    assert_eq!(classify(r#"        println!();"#), Write::Line);
    assert_eq!(
        classify(r#"print!("install {} and restart? [y/N] ", v);"#),
        Write::Prompt
    );
    assert_eq!(classify(r#"eprintln!("error: {err}");"#), Write::None);
    assert_eq!(classify(r#"eprint!("  allow? [y/N] ");"#), Write::None);
    assert_eq!(
        classify("// prints it with `println!` (ADR-0078)"),
        Write::None
    );
    assert_eq!(
        classify("/// `println!` panics on a closed pipe"),
        Write::None
    );
    assert_eq!(
        classify(r#"tracing::info!(provider = %name, "gui");"#),
        Write::None
    );
    // `println!` inside a longer name must not be mistaken for the macro.
    assert_eq!(classify("let reprint_counter = 0;"), Write::None);
}

/// A prompt asks the person at the keyboard a question. It belongs on stderr:
/// redirecting stdout must not hide the question and leave the command looking
/// hung. `update.rs` did exactly that.
#[test]
fn the_qc_binary_asks_its_questions_on_stderr() {
    let mut offending = Vec::new();
    for path in sources(&qc_src()) {
        let source = std::fs::read_to_string(&path).expect("read source");
        for (index, line) in source.lines().enumerate() {
            if classify(line) == Write::Prompt {
                offending.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "prompts must use `eprint!` so a redirected stdout cannot swallow them:\n{}",
        offending.join("\n")
    );
}

/// `println!` is only for products. A file that is not on the list above is
/// telling a story, and the story belongs on stderr (where `tracing` also puts
/// it in `logs/qc.log`, the only place a desktop launch can read it).
#[test]
fn only_product_files_print_to_stdout() {
    let allowed: BTreeSet<&str> = PRODUCT_FILES.iter().copied().collect();
    let src = qc_src();
    let mut offending = Vec::new();
    for path in sources(&src) {
        let relative = path
            .strip_prefix(&src)
            .expect("path under src")
            .to_string_lossy()
            .replace('\\', "/");
        if allowed.contains(relative.as_str()) {
            continue;
        }
        let source = std::fs::read_to_string(&path).expect("read source");
        for (index, line) in source.lines().enumerate() {
            if classify(line) != Write::None {
                offending.push(format!("{}:{}: {}", relative, index + 1, line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "these write to stdout but are not listed in PRODUCT_FILES — either send \
         them to `tracing`/`eprintln!`, or say what consumes the output:\n{}",
        offending.join("\n")
    );
}

/// Every listed file must still be there: a stale entry would quietly grant
/// stdout rights to a name someone reuses later.
#[test]
fn every_product_file_still_exists() {
    let src = qc_src();
    let present: BTreeSet<String> = sources(&src)
        .iter()
        .filter_map(|path| {
            let relative = path.strip_prefix(&src).ok()?;
            Some(relative.to_string_lossy().replace('\\', "/"))
        })
        .collect();
    let missing: Vec<&str> = PRODUCT_FILES
        .iter()
        .copied()
        .filter(|name| !present.contains(*name))
        .collect();
    assert!(
        missing.is_empty(),
        "PRODUCT_FILES lists files that no longer exist: {missing:?}"
    );
}

/// The GUI crate has no stdout consumer at all — its product is the window.
#[test]
fn the_gui_crate_does_not_print_to_stdout() {
    let root = Path::new(env!("CARGO_MANIFEST_DIR")).join("../qcode-gui/src");
    let mut offending = Vec::new();
    for path in sources(&root) {
        let source = std::fs::read_to_string(&path).expect("read source");
        for (index, line) in source.lines().enumerate() {
            if classify(line) != Write::None {
                offending.push(format!("{}:{}: {}", path.display(), index + 1, line.trim()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "the GUI crate must report through tracing, not stdout:\n{}",
        offending.join("\n")
    );
}
