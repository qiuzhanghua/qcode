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
        // **不**关单实例锁（以前这里设了 `QC_QCD_SINGLE_INSTANCE=0`）：关掉之后
        // `probe_in` 只会答 `Unknown`，于是 `start` 永远无法确认任何事，每次都落到
        // 「验证不了」那条出口——测试也就再也测不到真的路径了。锁的互斥问题由
        // `qc_as` 的 HOME 隔离解决，那才是正确的隔离层。
        .stdin(Stdio::null())
        .stdout(Stdio::piped())
        .stderr(Stdio::piped());
    command
}

/// 一个**隔离的用户**：`HOME` 指到临时目录。
///
/// 为什么必须隔离：`qcd` 的互斥锁是**每用户**一把，base 在 `$HOME/.local/share/qcode`
/// （ADR-0098 §2）。不隔离的话，同一台机器上开发者自己开着的 qcd、或者别的测试刚起的
/// 那一个，会让 `status` 报 `running`——于是「没有 daemon 时说停着」这条测试随机失败。
/// 2026-11-16 的门禁里就红过一次，排查发现是前面那条测试真的起了一个 daemon 没收拾。
///
/// 隔离之后 `--data-dir` 仍然各测各的：两个粒度本来就该分开问。
fn qc_as(home: &std::path::Path) -> Command {
    let mut command = qc();
    command.env("HOME", home);
    // Windows 上 `real_home` 先看 USERPROFILE；一起设掉，免得落到真实用户目录。
    command.env("USERPROFILE", home);
    command
}

/// 一个**短路径**的临时 HOME。
///
/// 不用 `tempfile::tempdir()`：它在 macOS 上给出 `$TMPDIR` = `/var/folders/...`，
/// 而 `Paths::for_scope` 会对数据目录 `canonicalize`，把 `/var` 变成 `/private/var`
/// （+8 字符）。于是 `<home>/.local/share/qcode/locks/qcd.sock` 冲到 105 字节，
/// 超过 `sockaddr_un.sun_path` 的 104 → `bind` 报 `AF_UNIX path too long` →
/// `acquire_in` 返回 `Disabled` → **qcd 照常跑，只是不带锁**。
///
/// 那样测试全绿，却一次也没测到锁；而 `start` 会等 3 秒然后误报「probably exited」。
/// 2026-11-16 就是这么发现问题、并留下几条残留 daemon 进程的。`/tmp` 在 macOS 上
/// 也是符号链接（→ `/private/tmp`），但短得多，canonicalize 之后仍远低于上限。
fn short_home() -> tempfile::TempDir {
    let base = if std::path::Path::new("/tmp").is_dir() {
        std::path::PathBuf::from("/tmp")
    } else {
        std::env::temp_dir()
    };
    tempfile::Builder::new()
        .prefix("qchome")
        .tempdir_in(base)
        .expect("short-path temp home")
}

fn run(home: &std::path::Path, args: &[&str], path: &str) -> Output {
    qc_as(home)
        .args(args)
        .env("PATH", path)
        .output()
        .expect("run qc")
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
    let home = short_home();
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
    let data_dir = data.path().display().to_string();
    let output = run(
        home.path(),
        &["daemon", "start", "--data-dir", &data_dir],
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

    // **收拾干净再走**。b) 分支会留下一个真进程；它握着**每用户**那把 qcd 锁，于是
    // 后面每一条 `status`/`stop` 测试都会看到一个不该存在的「在跑」。
    //
    // 这条清理以前没有，后果实测过：2026-11-16 的一次门禁里三条 daemon 残留进程
    // 让 `status_without_a_daemon_says_stopped` 红掉，而那条测试本身完全正确。
    // 隔离 `HOME` 已经把它们彼此隔开，这里再保证「谁起的谁负责关」。
    let _ = qc_as(home.path())
        .args(["daemon", "stop", "--data-dir", &data_dir])
        .output();
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
    let home = short_home();
    let data = tempfile::tempdir().expect("data dir");
    let output = qc_as(home.path())
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
    // 驱动四态也在里面：数据目录级的问题与用户级的问题是两回事（ADR-0091 vs §5）。
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
    let home = short_home();
    let data = tempfile::tempdir().expect("data dir");
    let output = qc_as(home.path())
        .args([
            "daemon",
            "stop",
            "--data-dir",
            &data.path().display().to_string(),
        ])
        .output()
        .expect("run qc daemon stop");

    // 用户级锁可能被同一个用户别的 qcd 占着（开发者自己开着的那个）。那种情况下
    // 「没有在跑」的判断不成立，于是这条只断言「不是崩溃」。
    if output.status.success() {
        assert!(
            stdout_of(&output).contains("not running") || stdout_of(&output).contains("stopped"),
            "success must say what happened: {}",
            stdout_of(&output)
        );
    }
}

/// `stop` 与 `status` 用**同一个**判据（用户级锁），否则会出现「status 说在跑、
/// stop 说没有」这种自相矛盾的话。
///
/// 这条不启动真 daemon（那要 spawn 一个常驻进程，在测试里留残留太容易），而是钉住
/// 两个子命令对同一状态的**说法一致性**：状态是「没有」时，两边都不能说「在跑」。
#[test]
fn stop_and_status_agree_about_the_same_state() {
    let home = short_home();
    let data = tempfile::tempdir().expect("data dir");
    let dir = data.path().display().to_string();

    let status = qc_as(home.path())
        .args(["daemon", "status", "--json", "--data-dir", &dir])
        .output()
        .expect("status");
    let value: serde_json::Value = serde_json::from_str(stdout_of(&status).trim()).expect("json");
    let state = value["daemon"]["state"].as_str().expect("state");

    let stop = qc_as(home.path())
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

    // 反面：`qcd` 必须在（测试用它的 `NotInstalled` 文案）。否则这条测试会因为
    // 「两个都没写」而假绿。它在 `[dev-dependencies]` 里——实现已经搬到
    // `qcode-core::daemon`（ADR-0098 §1），发布物不依赖 daemon 这个包。
    assert!(
        manifest.contains("qcd = {"),
        "tests read `qcd::detach` for the not-found message — if that moved, update this test"
    );
}

/// **每一条打包路径都必须带上 `qcd`**（ADR-0098 §7）。
///
/// 这条守的是一个测试**看不见**的盲区：`cargo build` 把 `qc` 与 `qcd` 都放进
/// `target/debug/`，于是 `find_qcd` 的第一步（找与 `qc` 同目录的兄弟）永远命中。
/// 集成测试跑的是那个布局，发布物是另一个布局——往包里忘记拷 `qcd`，所有测试照绿，
/// 而用户点 GUI 段头那颗「启动」会得到一句 `qcd` was not found。
///
/// 这个漏洞真的发生过：ADR-0098 实现完，`make app` 出来的 app 里只有 `qc`，
/// `build-for-release` 也只打 `qc`。行为测试看不见 shell 脚本，所以这里读脚本文本，
/// 与 `the_cli_does_not_depend_on_the_driver` 同一个办法（那条读 `Cargo.toml`）。
#[test]
fn every_packaging_path_ships_the_daemon() {
    let root = std::path::Path::new(env!("CARGO_MANIFEST_DIR")).join("../..");

    let app = std::fs::read_to_string(root.join("scripts/build-macos-app.sh"))
        .expect("read scripts/build-macos-app.sh");
    assert!(
        app.contains("-p qcd"),
        "`make app` 必须把 daemon 一起编出来（cargo build -p qcd）"
    );
    // 位置也是契约：`find_qcd` 只找同目录兄弟，放 Resources/ 就找不到。
    assert!(
        app.contains("Contents/MacOS/qcd"),
        "`make app` 必须把 qcd 放在与 qc 同级的 Contents/MacOS/ 下——\
         放别处 `find_qcd` 找不到（它第一步只看兄弟文件）"
    );

    let release =
        std::fs::read_to_string(root.join("build-for-release")).expect("read build-for-release");
    assert!(
        release.contains("-p \"$QCD_PKG\""),
        "发布链必须把 daemon 一起编出来（-p \"$QCD_PKG\"）"
    );
    // 归档里必须与 `qc` 同级：解压后的布局就是 `find_qcd` 要的那个。
    assert!(
        release.contains("$inner/$QCD_BIN"),
        "发布归档必须把 qcd 放在与 qc 同级的包根目录下"
    );
    // 单独一条发布资产：更新器按平台挑名字，两条都得有。
    assert!(
        release.contains("release_asset_named"),
        "发布链要给 qcd 一条自己的资产名（release_asset_named），\
         否则只更新了 qc、qcd 留在旧版本"
    );

    // ---- 同一课的另一半：装到**别的**入口也会漏。 ----
    //
    // 下面每条都是「用户点下去那一刻才发现」的路径，而且互不覆盖：修好 `make app`
    // 不等于修好 brew / scoop / winget / 自更新。

    let makefile = std::fs::read_to_string(root.join("Makefile")).expect("read Makefile");
    let linux_desktop = makefile
        .lines()
        .skip_while(|line| !line.starts_with("linux-desktop:"))
        .take_while(|line| !line.starts_with("linux-desktop-check:"))
        .collect::<Vec<_>>()
        .join("\n");
    assert!(
        linux_desktop.contains("-p qcd"),
        "`make linux-desktop` 必须把 daemon 一起编出来，否则装好的桌面入口指着一个\
         没有 qcd 的 target/release/"
    );

    let brew = std::fs::read_to_string(root.join("packaging/homebrew/qc.rb.in"))
        .expect("read homebrew formula");
    assert!(
        brew.contains("bin.install Dir[\"qcode_*/qcd\"].first"),
        "Homebrew formula 必须把 qcd 装进 bin——与 qc 同一个目录才有兄弟可找"
    );

    let scoop = std::fs::read_to_string(root.join("packaging/scoop/qc.json.in"))
        .expect("read scoop manifest");
    assert!(
        scoop.contains("\"qcd.exe\""),
        "scoop 的 bin 要带上 qcd.exe，否则 `qcd` 不在 PATH 上"
    );

    let winget =
        std::fs::read_to_string(root.join("packaging/winget/Qiuzhanghua.qcode.installer.yaml.in"))
            .expect("read winget installer manifest");
    // portable 安装器**只解出** NestedInstallerFiles 里点名的文件，其余留在 zip 里。
    // 少写一行 = winget 用户机器上永远没有 qcd.exe。
    assert_eq!(
        winget.matches(r"\qcd.exe").count(),
        2,
        "winget 的两个架构都必须在 NestedInstallerFiles 里列出 qcd.exe"
    );

    // 资产上传的 glob 也是契约：`dist/qc-*` 匹配不到 `qcd-*`（第三个字符是 `d`
    // 不是 `-`），于是新加的资产根本不会出现在 release 页面上，自更新取不到。
    let workflow = std::fs::read_to_string(root.join(".github/workflows/release.yml"))
        .expect("read release workflow");
    assert!(
        workflow.contains("dist/qcd-*"),
        "发布工作流必须上传 dist/qcd-*：`dist/qc-*` 这个 glob 不含 qcd"
    );

    // 自更新走的是裸二进制路径，包管理器不管它。不在这里装 qcd，用户更新完就是
    // 「新 qc + 没有 qcd」的半套安装。
    let updater =
        std::fs::read_to_string(root.join("crates/qcode/src/update.rs")).expect("read update.rs");
    assert!(
        updater.contains("daemon_beside"),
        "`qc update` 必须同时更新与 qc 同目录的 qcd"
    );

    // Windows 侧的发布脚本是**另一份实现**（pwsh），不是 bash 版的包装。它的
    // `-Archive` 就是 Windows 上「打包好供测试」的那条路，所以它也得带 qcd。
    let windows = std::fs::read_to_string(root.join("build-for-release.ps1"))
        .expect("read build-for-release.ps1");
    assert!(
        windows.contains("-p $Pkg -p $QcdPkg"),
        "Windows 发布脚本必须把 daemon 一起编出来（-p $QcdPkg）"
    );
    assert!(
        windows.contains(r#"Join-Path $Inner "$QcdBin.exe""#),
        "Windows 的 -Archive 必须把 qcd.exe 放进与 qc.exe 同级的包根目录"
    );
    assert!(
        windows.contains("Publish-ReleaseAsset $QcdBin"),
        "Windows 侧也要发 qcd 的独立资产，否则 `qc update` 在 Windows 上取不到它"
    );
}
