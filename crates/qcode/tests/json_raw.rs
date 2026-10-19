//! `--json` hides provider_raw unless --include-raw is passed.

use std::process::Command;

fn run(data: &std::path::Path, extra: &[&str]) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(["run", "hi", "--provider", "mock", "--json"])
        .args(extra)
        .args(["--data-dir", data.to_str().expect("utf-8 path")])
        .output()
        .expect("run qc");
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).into_owned()
}

#[test]
fn provider_raw_is_hidden_by_default_and_shown_on_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let plain = run(dir.path(), &[]);
    assert!(plain.contains("assistant_message"), "{plain}");
    assert!(
        !plain.contains("provider_raw"),
        "provider_raw leaked into default JSON:\n{plain}"
    );

    let raw_dir = tempfile::tempdir().expect("temp dir");
    let with_raw = run(raw_dir.path(), &["--include-raw"]);
    assert!(
        with_raw.contains("provider_raw"),
        "--include-raw did not include provider_raw:\n{with_raw}"
    );
}
