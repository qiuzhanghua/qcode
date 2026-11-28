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
        // 钉住语言（ADR-0105 §6）：`skills list` 的标题行现在跟随 `[ui] language` /
        // `QCODE_LANG`。技能名与 `[global]` 这类来源标签不翻，所以只有计数行要改。
        .env("QCODE_LANG", "zh-CN")
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
    assert!(stdout.contains("6 个技能"), "{stdout}");
}

/// ADR-0108 §7: this command must not connect to MCP servers, so it cannot
/// list what they serve — but it can say how many would contribute. Without
/// that line, turning `[skills] mcp = true` on and seeing nothing looks broken.
#[test]
fn points_at_servers_that_would_contribute_skills() {
    let home = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let config = workspace.path().join("config.toml");

    std::fs::write(
        &config,
        "[[mcp.servers]]\nname = \"github\"\ncommand = \"/bin/true\"\nskills = true\n",
    )
    .unwrap();
    let stdout = list(&config, workspace.path(), home.path());
    assert!(stdout.contains("另有 1 个 MCP 服务器"), "{stdout}");
    assert!(stdout.contains("qc doctor"), "{stdout}");

    // The default: a configured server without the per-server switch inherits
    // `[skills] mcp = false`, so there is nothing to explain.
    std::fs::write(
        &config,
        "[[mcp.servers]]\nname = \"github\"\ncommand = \"/bin/true\"\n",
    )
    .unwrap();
    let stdout = list(&config, workspace.path(), home.path());
    assert!(!stdout.contains("另有"), "{stdout}");
}

#[test]
fn reports_disabled_and_empty_discovery() {
    let home = TempDir::new().unwrap();
    let workspace = TempDir::new().unwrap();
    let config = workspace.path().join("config.toml");

    std::fs::write(&config, "").unwrap();
    let stdout = list(&config, workspace.path(), home.path());
    assert!(stdout.contains("没有发现技能"), "{stdout}");
    assert!(stdout.contains("[global]"), "{stdout}");
    assert!(stdout.contains("[user]"), "{stdout}");
    assert!(stdout.contains("[project]"), "{stdout}");

    std::fs::write(&config, "[skills]\nenabled = false\n").unwrap();
    let stdout = list(&config, workspace.path(), home.path());
    assert!(stdout.contains("技能已关闭"), "{stdout}");
}
