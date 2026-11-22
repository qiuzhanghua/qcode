//! 门禁 7：clap 帮助树的 key 可达性。
//!
//! `clap_i18n::localize` 按**路径**查表（`cli.cmd.<路径>.about` …）。这类 key 是
//! 运行期拼出来的，`qcode-i18n` 的静态扫描（门禁 4/6）看不见它们。没有这一条，
//! 两个方向的错误都不会被发现：
//!
//! - **漏写**：新加一个子命令忘了译文 → 中文界面里那一条静默保持英文；
//! - **写错**：key 拼错（`cli.cmd.schedul.create.about`）→ 声明了却永远查不到，
//!   变成死重量，同样静默。
//!
//! 所以这里遍历**真实的** `Command` 树，把 `localize` 会查的每一个 key 都算出来，
//! 与 `locales/*.yml` 里声明的 `cli.cmd.*` 做双向比对。
//!
//! **为什么住在 `src/` 而不是 `tests/`**：`Cli` 定义在 `qcode` 这个二进制 crate 里，
//! 集成测试（`tests/`）看不到二进制 crate 的私有项；同一目录下的 `#[cfg(test)]`
//! 模块则可以。代价是它不算「集成测试」，但门禁 2 的扫描器仍然跳过它
//! （`#[cfg(test)]` 之后的内容不计入基线）。

use clap::CommandFactory;
use std::collections::BTreeSet;

use crate::cli::Cli;

/// `localize` 会去查的所有 key（与 `clap_i18n::localize_at` 同构）。
fn reachable_keys() -> BTreeSet<String> {
    fn walk(cmd: &clap::Command, path: &str, out: &mut BTreeSet<String>) {
        let node = if path.is_empty() { "top" } else { path };
        out.insert(format!("cli.cmd.{node}.about"));
        out.insert(format!("cli.cmd.{node}.long_about"));
        for arg in cmd.get_arguments() {
            let id = arg.get_id().to_string();
            out.insert(format!("cli.cmd.{node}.arg.{id}.help"));
        }
        for sub in cmd.get_subcommands() {
            let next = if path.is_empty() {
                sub.get_name().to_string()
            } else {
                format!("{path}.{}", sub.get_name())
            };
            walk(sub, &next, out);
        }
    }
    let mut out = BTreeSet::new();
    walk(&Cli::command(), "", &mut out);
    out
}

/// `locales/*.yml` 里声明成 `cli.cmd.*` 的 key。
///
/// 用一个极小的 YAML 走查而不是引依赖：只需要「缩进 → 路径」，这些文件的结构就是
/// 纯粹的嵌套映射。叶子判定与门禁 1 一致（出现 `en` / `zh-CN` 即为叶子）。
fn declared_cmd_keys() -> BTreeSet<String> {
    let dir = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../qcode-i18n/locales");
    let mut keys = BTreeSet::new();
    let entries = std::fs::read_dir(&dir).expect("读不到 locales/");
    for entry in entries.flatten() {
        let path = entry.path();
        if !matches!(
            path.extension().and_then(|ext| ext.to_str()),
            Some("yml" | "yaml")
        ) {
            continue;
        }
        let text = std::fs::read_to_string(&path).expect("读不了译文文件");
        let mut stack: Vec<String> = Vec::new();
        for line in text.lines() {
            let trimmed = line.trim_end();
            if trimmed.is_empty() || trimmed.trim_start().starts_with('#') {
                continue;
            }
            let indent = trimmed.len() - trimmed.trim_start().len();
            let Some((key, value)) = trimmed.trim_start().split_once(':') else {
                continue;
            };
            stack.truncate(indent / 2);
            // 叶子优先判：`en` / `zh-CN` 行就算带了引号的值，也是叶子。
            if matches!(key, "en" | "zh-CN") {
                if stack.first().map(String::as_str) == Some("cli")
                    && stack.get(1).map(String::as_str) == Some("cmd")
                {
                    keys.insert(stack.join("."));
                }
                continue;
            }
            if value.trim().is_empty() {
                stack.push(key.to_string());
            }
        }
    }
    keys
}

/// 命令树里有 `///` 注释、却没有对应译文的地方。
fn collect_missing(
    cmd: &clap::Command,
    path: &str,
    declared: &BTreeSet<String>,
    missing: &mut Vec<String>,
) {
    let node = if path.is_empty() { "top" } else { path };
    let mut need = |field: String, present: bool| {
        if !present {
            return;
        }
        let key = format!("cli.cmd.{node}.{field}");
        if !declared.contains(&key) {
            missing.push(key);
        }
    };
    need("about".to_string(), cmd.get_about().is_some());
    need("long_about".to_string(), cmd.get_long_about().is_some());
    for arg in cmd.get_arguments() {
        let id = arg.get_id().to_string();
        // `help` / `version` 是 clap 自己加的，没有我们的话要说。
        if id == "help" || id == "version" || arg.get_help().is_none() {
            continue;
        }
        need(format!("arg.{id}.help"), true);
    }
    for sub in cmd.get_subcommands() {
        let next = if path.is_empty() {
            sub.get_name().to_string()
        } else {
            format!("{path}.{}", sub.get_name())
        };
        collect_missing(sub, &next, declared, missing);
    }
}

#[test]
fn every_help_key_is_reachable_and_declared() {
    let reachable = reachable_keys();
    let declared = declared_cmd_keys();
    assert!(!declared.is_empty(), "一条 cli.cmd.* 都没读到");

    // 方向一：声明了却**永远查不到**（拼错，或命令删了而译文留着）。
    let unreachable: Vec<&String> = declared.difference(&reachable).collect();
    assert!(
        unreachable.is_empty(),
        "这些 cli.cmd.* 声明了但命令树里查不到（key 拼错，或命令已删）：\n  - {}",
        unreachable
            .iter()
            .map(|key| key.as_str())
            .collect::<Vec<_>>()
            .join("\n  - ")
    );

    // 方向二：查得到却**没写译文**（中文界面里那一条静默保持英文）。
    let mut missing = Vec::new();
    collect_missing(&Cli::command(), "", &declared, &mut missing);
    assert!(
        missing.is_empty(),
        "命令树里有 `///` 注释但没有译文（中文界面会保持英文）：\n  - {}",
        missing.join("\n  - ")
    );
}
