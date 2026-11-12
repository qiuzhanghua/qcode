//! `qc gui` must not write to stdout.
//!
//! Two rules already said so:
//!
//! - `logging.rs`: "The JSONL event stream on stdout is a machine interface and
//!   must stay clean, so nothing here ever writes to stdout."
//! - `docs/design/LOGGING.md`: "stdout = 产物, stderr = 叙述."
//!
//! `commands/gui.rs` broke both with a bare `println!("gui: provider=… tools=…
//! skills=…")`. ADR-0079's sweep could not catch it: it classified `eprintln!`
//! call sites, because "stdout carries products" was taken as given. Nobody
//! audited the product channel for narration.
//!
//! The cost is not tidiness. A `.desktop` launch has no terminal, so the line is
//! discarded in exactly the situation where a user wants to know which provider
//! and how many tools the kernel resolved — the same reasoning that made
//! ADR-0077 add a log file in the first place.
//!
//! A source scan rather than a run: the startup path needs the `gui` feature and
//! a display, while this invariant must hold in the build everyone has.

/// True when `line` contains a `println!` that is not an `eprintln!`.
///
/// Comment lines are skipped: prose is allowed to name the macro it forbids.
fn has_bare_println(line: &str) -> bool {
    if line.trim_start().starts_with("//") {
        return false;
    }
    let bytes = line.as_bytes();
    let mut from = 0;
    while let Some(found) = line[from..].find("println!") {
        let at = from + found;
        // `eprintln!` ends in `println!`; only a bare one writes to stdout.
        if at == 0 || bytes[at - 1] != b'e' {
            return true;
        }
        from = at + "println!".len();
    }
    false
}

fn offending_lines(source: &str) -> Vec<String> {
    source
        .lines()
        .enumerate()
        .filter(|(_, line)| has_bare_println(line))
        .map(|(index, line)| format!("{}: {}", index + 1, line.trim()))
        .collect()
}

/// The scanner must be able to see what it is looking for, or this file's other
/// assertions pass for the wrong reason.
#[test]
fn the_scanner_sees_a_bare_println_and_ignores_an_eprintln() {
    assert!(has_bare_println(r#"println!("gui: provider={}");"#));
    assert!(has_bare_println(r#"    println!();"#));
    assert!(!has_bare_println(r#"eprintln!("error: {err}");"#));
    assert!(!has_bare_println("// prints it with `println!` (ADR-0078)"));
    assert!(!has_bare_println("/// `println!` panics on a closed pipe"));
    assert!(!has_bare_println(
        r#"tracing::info!(provider = %name, "gui");"#
    ));
}

/// The startup report goes to `tracing`, which also reaches `logs/qc.log`.
#[test]
fn the_gui_command_does_not_print_to_stdout() {
    let source = include_str!("../src/commands/gui.rs");
    let offending = offending_lines(source);
    assert!(
        offending.is_empty(),
        "`qc gui` must report through tracing, not stdout — a .desktop launch \
         has no terminal to read it from:\n{}",
        offending.join("\n")
    );
}

/// The same rule for the GUI crate: its window code has no stdout consumer at
/// all.
#[test]
fn the_gui_crate_does_not_print_to_stdout() {
    // The crate is scanned from its own directory listing so a new module cannot
    // slip past a hand-maintained file list.
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR"))
        .join("../qcode-gui/src")
        .canonicalize()
        .expect("qcode-gui source tree");
    let mut offending = Vec::new();
    let mut stack = vec![root.clone()];
    while let Some(dir) = stack.pop() {
        for entry in std::fs::read_dir(&dir).expect("read dir") {
            let path = entry.expect("dir entry").path();
            if path.is_dir() {
                stack.push(path);
                continue;
            }
            // Sibling test modules are not the shipped startup path.
            if path.extension().and_then(|e| e.to_str()) != Some("rs")
                || path
                    .file_name()
                    .and_then(|n| n.to_str())
                    .is_some_and(|n| n == "tests.rs")
            {
                continue;
            }
            let source = std::fs::read_to_string(&path).expect("read source");
            for line in offending_lines(&source) {
                offending.push(format!("{}: {line}", path.display()));
            }
        }
    }
    assert!(
        offending.is_empty(),
        "the GUI crate must not write to stdout:\n{}",
        offending.join("\n")
    );
}
