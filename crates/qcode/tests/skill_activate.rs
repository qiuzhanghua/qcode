//! `qc run --skill <name>` activates a skill by putting its instructions into
//! the user message; the event log keeps exactly what the model saw.

use std::path::Path;
use std::process::Command;

use serde_json::Value;
use tempfile::TempDir;

fn write_skill(workspace: &Path) {
    let dir = workspace.join(".qcode/skills/demo");
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        "---\nname: demo\ndescription: Demo skill for activation\n---\n\nFollow the demo steps.\n",
    )
    .unwrap();
}

fn run(workspace: &Path, home: &Path, skill: &str) -> std::process::Output {
    Command::new(env!("CARGO_BIN_EXE_qc"))
        .args([
            "run",
            "盘点本机",
            "--provider",
            "mock",
            "--skill",
            skill,
            "--json",
            "--data-dir",
        ])
        .arg(workspace.join("data"))
        .current_dir(workspace)
        .env("HOME", home)
        .output()
        .expect("run qc")
}

#[test]
fn skill_flag_puts_the_body_in_the_user_message() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    write_skill(workspace.path());

    let output = run(workspace.path(), home.path(), "demo");
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );

    let stdout = String::from_utf8_lossy(&output.stdout);
    let user = stdout
        .lines()
        .filter_map(|line| serde_json::from_str::<Value>(line).ok())
        .find(|event| event["type"] == "user_message")
        .expect("user_message event");
    let text = user["data"]["text"].as_str().expect("text");
    assert!(text.starts_with("[skill:demo]\n"), "{text}");
    assert!(text.contains("Follow the demo steps."), "{text}");
    assert!(text.ends_with("盘点本机"), "{text}");
}

#[test]
fn unknown_skill_fails_before_the_turn() {
    let workspace = TempDir::new().unwrap();
    let home = TempDir::new().unwrap();
    write_skill(workspace.path());

    let output = run(workspace.path(), home.path(), "nope");
    assert!(!output.status.success());
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(stderr.contains("unknown skill 'nope'"), "{stderr}");
    assert!(stderr.contains("demo"), "{stderr}");
}
