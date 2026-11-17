//! `qcd --version` / `--help`：输出形状，以及**一张纸都不留**。
//!
//! # 为什么这条测试要真跑二进制
//!
//! 版本问答的全部难点不在打印那一行，在**位置**（见 `crates/qcd/src/main.rs` 的
//! `early_exit`）：它必须排在任何初始化之前。只断言 stdout 的话，一个把
//! `--version` 留在 `run()` 里的实现照样能过——而它会在用户的数据目录里建出
//! `logs/`、写一对 `session starting` / `session ending`，并且（更糟）在正跑着
//! daemon 时被那把锁挡住。所以这里断言的是三件事：输出的**形状**、**没有一个文件
//! 被创建**、以及**有人持锁时照样能问**。
//!
//! 三个消费方都是脚本：`build-for-release` 拿产物的 `--version` 与文件名对账，
//! `qc update` 装完 `qcd` 读一次（`update::verify_daemon`），以及人手敲。

use std::path::{Path, PathBuf};
use std::process::{Command, Output};

/// 跑一次真实的 `qcd`，把它**所有**可能的写入点都圈在 `data_dir` 里。
///
/// `QCODE_DATA_DIR` 是 `data_dir_hint` 的第一个选择（`--data-dir` 之外的），所以
/// 日志会落进它；家目录类变量也指进同一个临时目录——万一哪天 `--version` 又开始
/// 读配置文件，测试也不该污染开发机的真实家目录。
fn qcd(args: &[&str], data_dir: &Path) -> Output {
    Command::new(env!("CARGO_BIN_EXE_qcd"))
        .args(args)
        .env("QCODE_DATA_DIR", data_dir)
        .env("HOME", data_dir.join("home"))
        .env("USERPROFILE", data_dir.join("home"))
        .env("XDG_DATA_HOME", data_dir.join("xdg"))
        .env("APPDATA", data_dir.join("roaming"))
        .output()
        .expect("qcd 应该能启动")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).trim().to_string()
}

/// 递归列出目录下的一切（文件与子目录）。空 Vec 就是「一个东西都没建」。
fn everything_under(dir: &Path) -> Vec<PathBuf> {
    let mut found = Vec::new();
    let mut stack = vec![dir.to_path_buf()];
    while let Some(next) = stack.pop() {
        let Ok(entries) = std::fs::read_dir(&next) else {
            continue;
        };
        for entry in entries.flatten() {
            let path = entry.path();
            if path.is_dir() {
                stack.push(path.clone());
            }
            found.push(path);
        }
    }
    found.sort();
    found
}

#[test]
fn version_is_the_package_version() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qcd(&["--version"], dir.path());

    assert!(output.status.success(), "--version 必须以 0 退出");
    // 形状与 `qc <version>` 一致，且**只有**这一行：`build-for-release` 用
    // `awk '{print $NF}'` 取版本，多一行就会被取错。
    assert_eq!(
        stdout_of(&output),
        format!("qcd {}", env!("CARGO_PKG_VERSION"))
    );

    // 这条才是本测试的主要断言：只问「你是谁」不该留下现场。
    assert_eq!(
        everything_under(dir.path()),
        Vec::<PathBuf>::new(),
        "--version 不该在数据目录里建出任何东西（日志、锁、配置都不行）"
    );
}

#[test]
fn capital_v_is_the_same_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qcd(&["-V"], dir.path());
    assert!(output.status.success());
    assert_eq!(
        stdout_of(&output),
        format!("qcd {}", env!("CARGO_PKG_VERSION"))
    );
}

#[test]
fn lowercase_v_is_not_a_version_request() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qcd(&["-v"], dir.path());

    // `-v` 留给将来的 verbose。现在它必须是个错，而不是一个顺便答了的版本——
    // 否则等 verbose 真做出来时，脚本里那句 `qcd -v` 会突然从「报版本」变成
    // 「跑起来并刷屏」，而没有人会想到去改它。
    assert!(!output.status.success(), "-v 不该被当成 --version");
    assert!(
        !stdout_of(&output).starts_with("qcd "),
        "-v 的 stdout 不该长得像版本"
    );
    let stderr = String::from_utf8_lossy(&output.stderr);
    assert!(
        stderr.contains("unknown argument"),
        "-v 应该被明确拒绝，实际 stderr: {stderr}"
    );
    // 说一次就够：`qcd: qcd: unknown argument` 是同一个前缀被加了两次。
    assert!(
        !stderr.contains("qcd: qcd:"),
        "错误信息不该有双重 `qcd:` 前缀，实际: {stderr}"
    );
}

/// `--help` 曾经排在日志初始化之后：它会先建出 `logs/` 再打印帮助。这条钉住那次
/// 搬迁——帮助和版本是同一个问题（只问不写），所以放在一起验。
#[test]
fn help_is_answered_without_leaving_a_trace() {
    let dir = tempfile::tempdir().expect("temp dir");
    let output = qcd(&["--help"], dir.path());

    assert!(output.status.success());
    let text = stdout_of(&output);
    assert!(text.contains("Usage: qcd"), "帮助里要有用法行: {text}");
    // 帮助自己得说全：`--version` 是新加的，漏了它就等于没有。
    assert!(
        text.contains("--version"),
        "帮助的用法行要提到 --version: {text}"
    );
    assert_eq!(
        everything_under(dir.path()),
        Vec::<PathBuf>::new(),
        "--help 不该在数据目录里建出任何东西"
    );
}

/// **正跑着一个 qcd 时，磁盘上这个版本的问答必须仍然可用。**
///
/// 这是 `--version` 位置上的第二个约束：它必须早于 `acquire_in`。否则 `qc update`
/// 换完文件想对账，会被那个「表示谁在跑」的锁挡住——而它恰恰是最想知道版本的时刻。
///
/// 测试自己拿下那把锁（不 `serve`，所以没有人应答），然后跑子进程。如果实现真的
/// 走到 `acquire_in`，子进程会在约 1 秒后报错退出，断言随之失败——不会挂住。
#[test]
fn version_answers_while_someone_holds_the_user_lock() {
    use qcode_core::single_instance::{Outcome, Scope};

    let dir = tempfile::tempdir().expect("temp dir");
    let lock =
        match qcode_core::single_instance::acquire_in(Scope::QCD, dir.path()).expect("acquire") {
            Outcome::Primary(primary) => primary,
            // 锁机制在这个环境里不可用（容器、网络文件系统）：没有可验的东西。
            // 这不是失败——`acquire_in` 自己在那种情况下也只会给 `Disabled`。
            other => {
                eprintln!("跳过：这把锁在这里不可用（{other:?}）");
                return;
            }
        };

    let output = qcd(&["--version"], dir.path());
    assert!(
        output.status.success(),
        "有人持锁时 --version 也必须答（实际: {}）",
        String::from_utf8_lossy(&output.stderr)
    );
    assert_eq!(
        stdout_of(&output),
        format!("qcd {}", env!("CARGO_PKG_VERSION"))
    );

    drop(lock);
}
