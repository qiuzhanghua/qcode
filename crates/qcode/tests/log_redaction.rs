//! The log file must never contain a resolved API key (ADR-0078).
//!
//! A unit test cannot cover this. The protection is a formatter in the `qc` binary
//! fed by a secret set installed in `qcode_kernel::assemble`, and the thing worth
//! asserting is that those two are actually wired together — which is exactly what
//! removing the `redact::install(...)` line from `assemble` breaks, silently, in
//! every other test.
//!
//! So this runs the real binary against a provider that cannot possibly answer: the
//! base URL carries the key in its query string (a shape real providers use) and
//! points at a closed port, so the engine logs the failing URL and exits at once.
//! No network, no credentials, no timing.

use std::path::{Path, PathBuf};
use std::process::Command;

/// Long enough to pass the minimum length, and distinctive enough that a false
/// pass cannot come from an unrelated `<redacted>` elsewhere in the file.
const KEY: &str = "sk-integration-MUST-NOT-BE-LOGGED-5f2c8a";

#[test]
fn the_api_key_never_reaches_the_log_file() {
    let base = Path::new(env!("CARGO_TARGET_TMPDIR"))
        .join(format!("log-redaction-{}", std::process::id()));
    let data = base.join("data");
    std::fs::create_dir_all(&data).expect("data dir");
    let config = base.join("config.toml");
    std::fs::write(&config, fixture()).expect("config");

    // Port 9 (discard) is not listening, so the turn fails as fast as a connection
    // can be refused. `info` is what makes `run` log the resolved base URL.
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .args(["--data-dir"])
        .arg(&data)
        .args(["--config"])
        .arg(&config)
        .args(["run", "hello"])
        .env("E2E_KEY", KEY)
        .env("RUST_LOG", "info")
        .env_remove("QCODE_DATA_DIR")
        // Any inherited key would be installed as well; harmless, but keep the run
        // to exactly the secret this test asserts about.
        .env_remove("DEEPSEEK_API_KEY")
        .env_remove("OPENAI_API_KEY")
        .output()
        .expect("run qc");

    let log = newest_log(&data).unwrap_or_else(|| {
        panic!(
            "no log file in {}; stderr was: {}",
            data.join("logs").display(),
            String::from_utf8_lossy(&output.stderr)
        )
    });
    let text = std::fs::read_to_string(&log).expect("read log");

    // Not vacuous: the run really did log the URL that carries the key. Without
    // this the absence assertion below would pass on an empty file.
    assert!(
        text.contains("base_url="),
        "the resolved base URL was never logged, so this test proves nothing:\n{text}"
    );
    assert!(
        text.contains("<redacted>"),
        "nothing was redacted, so the secret set was never installed:\n{text}"
    );
    assert!(
        !text.contains(KEY),
        "the API key reached {}:\n{text}",
        log.display()
    );

    // The error printed by `main` (`eprintln!`) bypasses `tracing` entirely, so it
    // is masked by the two hand-written call sites instead. Same requirement.
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        !stderr.contains(KEY),
        "the API key reached stderr:\n{stderr}"
    );
}

/// The newest `qc.*.log`, since the appender names files by date.
fn newest_log(data: &Path) -> Option<PathBuf> {
    std::fs::read_dir(data.join("logs"))
        .ok()?
        .filter_map(Result::ok)
        .map(|entry| entry.path())
        .filter(|path| {
            path.file_name()
                .and_then(|name| name.to_str())
                .is_some_and(|name| name.starts_with("qc.") && name.ends_with(".log"))
        })
        .max()
}

fn fixture() -> String {
    format!(
        r#"
# The key travels in the URL, which is the shape that made the leak possible in
# the first place: `?base_url` is logged, so a formatter-level filter is the only
# place that can catch it.
[providers.e2e]
base_url = "http://127.0.0.1:9/v1?key={KEY}"
api_key_env = "E2E_KEY"
default_model = "e2e-model"

[default-model]
provider = "e2e"
model = "e2e-model"
"#
    )
}
