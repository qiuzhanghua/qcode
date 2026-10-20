//! `qc skills list` walks the configured roots and reports what it found.

use std::path::Path;
use std::process::Command;

use tempfile::TempDir;

fn write_skill(root: &Path, name: &str, description: &str) {
    let dir = root.join(name);
    std::fs::create_dir_all(&dir).unwrap();
    std::fs::write(
        dir.join("SKILL.md"),
        format!("---\nname: {name}\ndescription: {description}\n---\n\nbody\n"),
    )
    .unwrap();
}

fn list(config: &Path, workspace: &Path, home: &Path) -> String {
    let output = Command::new(env!("CARGO_BIN_EXE_qc"))
        .arg("skills")
        .arg("list")
        .arg("--config")
        .arg(config)
        .current_dir(workspace)
        .env("HOME", home)
        .output()
        .expect("run qc");
    assert!(
        output.status.success(),
        "qc failed: {}",
        String::from_utf8_lossy(&output.stderr)
    );
    String::from_utf8_lossy(&output.stdout).to_string()
}

#[test]
fn lists_global_configured_and_project_roots() {
    let home = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let extra = TempDir::new().unwrap();

    write_skill(
        &home.path().join(".agents/skills"),
        "global-one",
        "from the npx skills global root",
    );
    write_skill(
        &home.path().join(".config/agents/skills"),
        "global-two",
        "from the universal agent root",
    );
    write_skill(extra.path(), "delta", "from an absolute config root");
    write_skill(
        &home.path().join("home-skills"),
        "tilde",
        "from a tilde config root",
    );
    write_skill(
        &workspace.path().join(".agents/skills"),
        "project-agents",
        "from the project agents root",
    );
    write_skill(
        &workspace.path().join(".qcode/skills"),
        "project-skill",
        "from the qcode project root",
    );

    let config = workspace.path().join("config.toml");
    std::fs::write(
        &config,
        // Literal strings: a Windows path's backslashes are not escapes there.
        format!(
            "[skills]\nroots = ['{}', '~/home-skills']\n",
            extra.path().display()
        ),
    )
    .unwrap();

    let stdout = list(&config, workspace.path(), home.path());
    assert!(stdout.contains("global-one  [global]"), "{stdout}");
    assert!(stdout.contains("global-two  [global]"), "{stdout}");
    assert!(stdout.contains("delta  [config]"), "{stdout}");
    assert!(stdout.contains("tilde  [config]"), "{stdout}");
    assert!(stdout.contains("project-agents  [project]"), "{stdout}");
    assert!(stdout.contains("project-skill  [project]"), "{stdout}");
    assert!(stdout.contains("6 skill(s)"), "{stdout}");
}

#[test]
fn reports_disabled_and_empty_discovery() {
    let home = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let config = workspace.path().join("config.toml");

    std::fs::write(&config, "").unwrap();
    let stdout = list(&config, workspace.path(), home.path());
    assert!(stdout.contains("no skills discovered"), "{stdout}");
    assert!(stdout.contains("[global]"), "{stdout}");
    assert!(stdout.contains("[user]"), "{stdout}");
    assert!(stdout.contains("[project]"), "{stdout}");

    std::fs::write(&config, "[skills]\nenabled = false\n").unwrap();
    let stdout = list(&config, workspace.path(), home.path());
    assert!(stdout.contains("skills are disabled"), "{stdout}");
}
