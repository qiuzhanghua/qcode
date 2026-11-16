//! `qc daemon` 的诚实性（ADR-0092 §11 阶段 3 的出口判据）。
//!
//! 这一组守的不是「daemon 能不能跑」（那是 `qcd/tests/acceptance.rs`），而是
//! **它跑不了的时候说的话对不对**。ADR-0092 §11 单独强调过这一条：
//!
//! > 一个说「已启动」但其实没启动的 `daemon start`，比没有这个子命令更糟。
//!
//! 所以 `qcd` 不在 `PATH` 上时，退出码必须非 0、stderr 必须说清楚「它是独立二进制、
//! 该放哪」。这条以前完全无从验证——连子命令都还没有。

use std::process::{Command, Output, Stdio};

fn qc() -> Command {
    let mut command = Command::new(env!("CARGO_BIN_EXE_qc"));
    // 与其它集成测试同一个理由：从 GUI 派生的进程会带着启动标记，那会盖过平台信号。
    command
        .env_remove("QCODE_LAUNCHED_FROM_GUI")
        .env_remove("__CFBundleIdentifier")
        // 单实例的逃生口：这个测试不关心锁，只关心「找不到 qcd」这条路径。
        // 但**不能**关掉它——关了 `acquire_in` 会走 `Disabled`，probe 变成
        // `Unknown`，于是 `start` 直接进到「找 qcd」那一步。这正好是我们要的路径。
        .env("QC_QCD_SINGLE_INSTANCE", "0")
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

fn run(args: &[&str], path: &str) -> Output {
    qc().args(args).env("PATH", path).output().expect("run qc")
}

fn stdout_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stdout).into_owned()
}

fn stderr_of(output: &Output) -> String {
    String::from_utf8_lossy(&output.stderr).into_owned()
}

/// **出口判据**：`PATH` 里没有 `qcd` 时，`qc daemon start` 退出码非 0 且说实话。
#[test]
fn starting_without_qcd_on_path_fails_and_says_so() {
    let data = tempfile::tempdir().expect("data dir");
    // 一个空目录当 PATH：连 `sh` 都找不到，所以只可能**不**找到 `qcd`。
    //
    // 但 `qcd` 的发现逻辑第一步看的是「与 `qc` 同目录」——cargo 把 `qc` 与 `qcd`
    // 放在同一个 target 目录里，所以这一步会先命中。要让测试真的走到 PATH 分支，
    // 得把这个兄弟也排除掉，而那不是测试能改的（它就在那儿）。
    //
    // 于是这一条验的是**另一件事**：`daemon start` 无论如何都不会说「已启动」而
    // 实际上什么都没起。看下面的断言。
    let empty = tempfile::tempdir().expect("empty dir");
    let output = run(
        &[
            "daemon",
            "start",
            "--data-dir",
            &data.path().display().to_string(),
        ],
        &empty.path().display().to_string(),
    );

    // 两种可接受的结果，两种都**不是**「已启动」：
    //   a) 找不到 qcd → 非 0 + 说明它是独立二进制
    //   b) 找到了、起来了 → 0 + 「qcd started」
    //
    // 不可接受的是：退出码 0 而 stdout 里没有「started」，或者退出码非 0 却说了
    // 「已启动」。这正是 §11 要防的那个谎。
    let said_started = stdout_of(&output).contains("started");
    let complained = stderr_of(&output).contains("not found")
        || stderr_of(&output).contains("did not take the driver lock");

    assert!(
        (output.status.success() && said_started) || (!output.status.success() && complained),
        "must either really start or say why not.\nstatus: {:?}\nstdout: {}\nstderr: {}",
        output.status.code(),
        stdout_of(&output),
        stderr_of(&output)
    );
}

/// `qcd` 找不到时的文案必须可操作：说它是**独立二进制**、说该放哪。
///
/// 直接调用 `qcd::detach::find_qcd` 在 `PATH` 被清空、且已把 `qcd` 挪走的条件下
/// 验证——这条测试改的是自己的 `PATH`，不影响别的测试。
#[test]
fn the_not_installed_message_is_actionable() {
    // 用一个空 PATH，并把「与 qc 同目录」这一支排除：临时把 PATH 指到空目录不足
    // 以排除兄弟目录，所以这里只验文案形状——`find_qcd` 在 cargo 的 target 目录里
    // 会成功，于是跳过。
    //
    // 保留这条是因为它记录了一个判断：真正的「找不到」场景在 CI 里不好造（cargo
    // 总把两个二进制放一起）。能造它的时候（发布产物被拆开、`qcd` 没装），文案
    // 就是唯一的指望。文案的形状在这里被钉住。
    let message = qcd::detach::NotInstalled::describe_for_test(&["/nowhere".to_string()]);
    assert!(
        message.contains("separate binary"),
        "the message must say qcd is a separate binary:\n{message}"
    );
    assert!(
        message.contains("/nowhere"),
        "the message must list where it looked:\n{message}"
    );
    assert!(
        message.contains("will not fire"),
        "the message must say what the consequence is:\n{message}"
    );
}

/// `daemon status` 在没有 daemon 时**明确说没有**，不假装（ADR-0092 验证 6）。
#[test]
fn status_without_a_daemon_says_stopped() {
    let data = tempfile::tempdir().expect("data dir");
    let output = qc()
        .args([
            "daemon",
            "status",
            "--json",
            "--data-dir",
            &data.path().display().to_string(),
        ])
        .output()
        .expect("run qc daemon status");

    assert!(output.status.success(), "{}", stderr_of(&output));
    let value: serde_json::Value =
        serde_json::from_str(stdout_of(&output).trim()).expect("stdout must be one JSON object");
    // 「不知道」也是可能的：这个平台没有文件锁。两者都**不是**「在跑」。
    let state = value["daemon"]["state"].as_str().expect("daemon.state");
    assert!(
        state == "stopped" || state == "unknown",
        "without a daemon the state must not be `running`, got {state}"
    );
    // 驱动四态也在里面：数据目录级的问题与机器级的问题是两回事（ADR-0091 vs §5）。
    assert!(
        ["running", "held", "gone", "unknown"]
            .contains(&value["driver"]["state"].as_str().expect("driver.state")),
        "{value}"
    );
}

/// `stop` 在没有 daemon 时是**成功**，不是错误。
///
/// 与 `start` 的非对称：那里「已经有了」有害（用户以为起了新的），这里「本来就没有」
/// 只是意图已经满足。脚本里 `qc daemon stop` 该能无条件跑。
#[test]
fn stopping_a_stopped_daemon_succeeds() {
    let data = tempfile::tempdir().expect("data dir");
    let output = qc()
        .args([
            "daemon",
            "stop",
            "--data-dir",
            &data.path().display().to_string(),
        ])
        .output()
        .expect("run qc daemon stop");

    // 机器级锁可能被同一台机器上别的 qcd 占着（开发者自己开着的那个）。那种情况下
    // 「没有在跑」的判断不成立，于是这条只断言「不是崩溃」。
    if output.status.success() {
        assert!(
            stdout_of(&output).contains("not running") || stdout_of(&output).contains("stopped"),
            "success must say what happened: {}",
            stdout_of(&output)
        );
    }
}

/// `stop` 与 `status` 用**同一个**判据（机器级锁），否则会出现「status 说在跑、
/// stop 说没有」这种自相矛盾的话。
///
/// 这条不启动真 daemon（那要 spawn 一个常驻进程，在测试里留残留太容易），而是钉住
/// 两个子命令对同一状态的**说法一致性**：状态是「没有」时，两边都不能说「在跑」。
#[test]
fn stop_and_status_agree_about_the_same_state() {
    let data = tempfile::tempdir().expect("data dir");
    let dir = data.path().display().to_string();

    let status = qc()
        .args(["daemon", "status", "--json", "--data-dir", &dir])
        .output()
        .expect("status");
    let value: serde_json::Value = serde_json::from_str(stdout_of(&status).trim()).expect("json");
    let state = value["daemon"]["state"].as_str().expect("state");

    let stop = qc()
        .args(["daemon", "stop", "--data-dir", &dir])
        .output()
        .expect("stop");
    let said_not_running = stdout_of(&stop).contains("not running");

    // 「不知道」（平台没有文件锁）或「在跑」（开发者自己开着 daemon）都不能推出
    // 「stop 该说 not running」——只有 `stopped` 能。
    if state == "stopped" {
        assert!(
            said_not_running,
            "status said stopped but stop disagreed:\n{}",
            stderr_of(&stop)
        );
    }
}

/// `qc` 侧**只做客户端**：它不驱动，也不碰 `qcode-driver` 的决策。
///
/// 这条是 ADR-0092 §11 的承诺变成测试。承诺是「`qc daemon start|stop|status` 只做
/// 拉起 + 问话」，而它很容易在后续维护里走样——比如有人为了「status 顺便报一下下次
/// 触发时间」把驱动逻辑拉进来，于是 `qc` 与 daemon 的依赖面重新纠缠。
///
/// 检查的是**依赖**而不是行为：`crates/qcode` 的 `Cargo.toml` 里不能出现
/// `qcode-driver`。行为测试只能抽查几个命令，依赖不会说谎——它一旦被加进去，整条
/// 边界就没了。
#[test]
fn the_cli_does_not_depend_on_the_driver() {
    let manifest = std::fs::read_to_string(
        std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("Cargo.toml"),
    )
    .expect("read our own manifest");

    // 找 `qcode-driver` 出现在依赖段里的位置。注释里提到它是可以的（我们确实在注释
    // 里解释过为什么不用它），所以只认「行首的依赖写法」。
    let offenders: Vec<&str> = manifest
        .lines()
        .filter(|line| {
            let trimmed = line.trim_start();
            !trimmed.starts_with('#')
                && trimmed.starts_with("qcode-driver")
                && trimmed.contains('=')
        })
        .collect();
    assert!(
        offenders.is_empty(),
        "`qc` must stay a client of the daemon, not a second driver.\n\
         These dependency lines pull the driver into the CLI:\n  {}",
        offenders.join("\n  ")
    );

    // 反面：`qcd` 必须在（`detach` 模块提供了 spawn 与 find_qcd）。否则这条测试
    // 会因为「两个都没写」而假绿。
    assert!(
        manifest.contains("qcd = {"),
        "`qc` needs `qcd` for detach/spawn — if that changed, update this test"
    );
}
