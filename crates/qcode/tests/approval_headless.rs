//! Headless regression for the approval demo: without a TTY the default
//! approver denies, the denial comes back as a tool result, and the model is
//! allowed to continue instead of the turn aborting.
//!
//! The two tests below are a pair — same call, one flag apart. `AutoDeny` and
//! `AutoApprove` are chosen by that flag in `crates/qcode/src/commands/run.rs`,
//! and testing only the deny half would leave the other one free to break
//! silently.

use std::process::{Command, Stdio};

/// Run the approval demo headlessly and return stdout.
fn run_headless(extra: &[&str]) -> String {
    let dir = tempfile::tempdir().expect("temp dir");
    let mut args = vec![
        "run",
        "demo",
        "--provider",
        "mock-approval",
        "--json",
        "--data-dir",
        dir.path().to_str().expect("utf-8 path"),
    ];
    args.extend_from_slice(extra);

    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        // 断言比对的是 mock provider 的夹具文本，而夹具跟着 UI 语言走
        // （I18N.md 不变量 5：测试进程钉 `zh-CN`）。
        .env("QCODE_LANG", "zh-CN")
        .args(&args)
        .stdin(Stdio::null())
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
fn non_tty_denies_and_the_model_continues() {
    let stdout = run_headless(&[]);

    assert!(
        stdout.contains(r#""type":"approval_requested""#),
        "no approval request in:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""decision":"deny""#),
        "default approver did not deny in:\n{stdout}"
    );
    assert!(
        stdout.contains("denied: default: sandbox escalation"),
        "denial was not reported as a tool result in:\n{stdout}"
    );
    // The second scripted response still runs: denial is data, not a crash.
    assert!(
        stdout.contains("审批流程演示结束。"),
        "the model did not continue after denial in:\n{stdout}"
    );
    assert!(
        stdout.contains(r#""status":"completed""#),
        "turn did not complete:\n{stdout}"
    );
}

/// The other half of the pair above.
///
/// Without this, `AutoApprove` could break and everything would stay green:
/// the deny test only ever exercises `AutoDeny`. The two runs differ by one
/// flag, so together they also pin down that `--yes` is what moves the decision.
#[test]
fn yes_switches_the_same_call_from_deny_to_allow() {
    let stdout = run_headless(&["--yes"]);

    assert!(
        stdout.contains(r#""decision":"allow""#),
        "--yes did not approve:\n{stdout}"
    );
    assert!(
        !stdout.contains(r#""decision":"deny""#),
        "--yes still denied something:\n{stdout}"
    );
    // `run_shell` actually ran, so the approved command's output appears.
    assert!(
        stdout.contains("qcode-approval-demo"),
        "the approved command did not run:\n{stdout}"
    );
}
