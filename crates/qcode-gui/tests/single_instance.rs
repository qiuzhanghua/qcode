//! 单实例与跨进程激活（ADR-0083 / ADR-0084）。
//!
//! 这些用例要真的开两个进程：`flock` 的语义、残留 socket 被谁收、`kill -9`
//! 之后锁会不会被内核放掉——全都无法在单进程里假装。子进程就是本测试二进制
//! 自己，带一个环境变量进去走 [`helper_holds_the_lock`] 这条分支。
//!
//! 只有持有**那把锁**的进程才算第一个实例；这一点决定了子进程必须真的
//! `acquire` 一次，而不是往磁盘上写一个文件冒充。

#![cfg(unix)]

use std::path::{Path, PathBuf};
use std::process::{Child, Command};
use std::time::{Duration, Instant};

use qcode_gui::SingleInstanceOutcome;
use qcode_gui::single_instance;

/// 子进程模式的开关：设了它，子进程就抢锁、报「准备好了」、然后一直等。
const HELPER_DIR: &str = "QC_SI_HELPER_DIR";
const HELPER_READY: &str = "QC_SI_HELPER_READY";
/// 让子进程只拿锁、**不**托管通道——用来把「锁」和「通道」分开验。
const HELPER_NO_CHANNEL: &str = "QC_SI_HELPER_NO_CHANNEL";
const HELPER_NAME: &str = "helper_holds_the_lock";

/// 子进程那一半。在普通跑测试时它立刻返回（没人设环境变量）。
#[test]
fn helper_holds_the_lock() {
    let Some(dir) = std::env::var_os(HELPER_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let outcome = single_instance::acquire(&dir).expect("helper 应当能拿到锁");
    let SingleInstanceOutcome::Primary(primary) = outcome else {
        panic!("helper 没拿到锁");
    };
    // 托管通道。接收端留着不读——协议只有一件事，读不读都不影响回复。
    //
    // `QC_SI_HELPER_NO_CHANNEL` 让这一步不发生：锁被占、却没有人在通道上
    // 应答，正是 ADR-0083 §4 那一行要处理的状态。
    if std::env::var_os(HELPER_NO_CHANNEL).is_none() {
        let (tx, rx) = std::sync::mpsc::channel();
        primary.serve(tx);
        std::mem::forget(rx);
    } else {
        // 锁必须继续被持有（`primary` 不能掉），所以别把它提前丢掉。
        std::mem::forget(primary);
    }

    // 报「准备好了」。父进程据此知道可以开始试探了，而不是靠 sleep 猜。
    if let Some(ready) = std::env::var_os(HELPER_READY) {
        std::fs::write(&ready, b"ready").expect("写 ready 标记");
    }
    // 持锁到死：父进程用完会 kill 掉我们。
    std::thread::sleep(Duration::from_secs(120));
}

/// 起一个「已经占着这个 data dir」的子进程，返回它和它的 ready 标记。
fn spawn_holder(dir: &Path) -> (Child, PathBuf) {
    spawn_holder_with(dir, false)
}

fn spawn_holder_with(dir: &Path, no_channel: bool) -> (Child, PathBuf) {
    let ready = dir.with_extension("ready");
    let _ = std::fs::remove_file(&ready);
    let mut command = Command::new(std::env::current_exe().expect("测试二进制路径"));
    command
        .args(["--exact", HELPER_NAME, "--nocapture"])
        .env(HELPER_DIR, dir)
        .env(HELPER_READY, &ready)
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null());
    // 只在需要时设这个变量：`env(K, "")` 也是「存在」，子进程那边用
    // `var_os(..).is_none()` 判断，会把它当成「设了」。
    if no_channel {
        command.env(HELPER_NO_CHANNEL, "1");
    }
    let child = command.spawn().expect("起子进程");
    wait_until(|| ready.exists(), Duration::from_secs(20));
    (child, ready)
}

fn wait_until(mut done: impl FnMut() -> bool, timeout: Duration) -> bool {
    let started = Instant::now();
    while started.elapsed() < timeout {
        if done() {
            return true;
        }
        std::thread::sleep(Duration::from_millis(20));
    }
    done()
}

/// 第一个实例在跑时，第二次启动必须**让位**，而不是开第二个。
///
/// 这条同时验了两件事：锁被占（否则两个都拿到 `Primary`），以及通道能连上并
/// 得到回复（否则会落到「锁被占但没人应答」的超时分支而报错）。
#[test]
fn a_second_instance_hands_over_instead_of_starting() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let (mut holder, ready) = spawn_holder(&dir);
    let outcome = single_instance::acquire(&dir);
    let _ = std::fs::remove_file(&ready);
    let killed = holder.kill();
    let _ = holder.wait();

    let outcome = outcome.expect("第二次启动不该报错");
    assert!(
        matches!(outcome, SingleInstanceOutcome::HandedOver),
        "第二个实例应当让位，实际是 {}",
        describe(&outcome)
    );
    assert!(killed.is_ok(), "子进程应当被收掉");
}

/// 不同 `--data-dir` 是两个互不相干的实例，必须都能起来。
///
/// 这不是漏洞：两个 data dir 写两份 store，合成一个语义才是错的。而默认
/// data dir 只有一个，所以「每机每用户一个」在默认配置下自然成立。
#[test]
fn different_data_dirs_both_start() {
    let tmp = tempfile::tempdir().unwrap();
    let a = tmp.path().join("a");
    let b = tmp.path().join("b");
    std::fs::create_dir_all(&a).unwrap();
    std::fs::create_dir_all(&b).unwrap();

    let first = single_instance::acquire(&a).expect("a 应当拿到锁");
    assert!(matches!(first, SingleInstanceOutcome::Primary(_)));
    let second = single_instance::acquire(&b).expect("b 应当拿到锁");
    assert!(
        matches!(second, SingleInstanceOutcome::Primary(_)),
        "不同 data dir 必须都能起来，实际是 {}",
        describe(&second)
    );
}

/// 逃生口：锁机制坏了（或用户就是要多开）时不许把人锁在门外。
#[test]
fn the_escape_hatch_skips_the_check() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();
    let _held = single_instance::acquire(&dir).expect("第一次");

    // 环境变量是进程级的，与并行跑的其它用例共享——所以这条用子进程验，
    // 不去动本进程的环境（`std::env::set_var` 在测试里是竞态源）。
    let output = Command::new(std::env::current_exe().unwrap())
        .args(["--exact", "escape_hatch_child", "--nocapture"])
        .env("QC_GUI_SINGLE_INSTANCE", "0")
        .env(HELPER_DIR, &dir)
        .output()
        .expect("起子进程");
    assert!(
        output.status.success(),
        "子进程失败：{:?}",
        String::from_utf8_lossy(&output.stderr)
    );
}

/// [`the_escape_hatch_skips_the_check`] 的子进程那一半。
#[test]
fn escape_hatch_child() {
    let Some(dir) = std::env::var_os(HELPER_DIR) else {
        return;
    };
    let dir = PathBuf::from(dir);
    let outcome = single_instance::acquire(&dir).expect("不该报错");
    match outcome {
        SingleInstanceOutcome::Disabled(_) => {}
        other => panic!(
            "设了 QC_GUI_SINGLE_INSTANCE=0 仍被互斥拦下：{}",
            describe(&other)
        ),
    }
}

/// `kill -9` 之后锁必须由内核放掉，而不是被一个残留文件卡死。
///
/// ADR-0083 §1 选 OS 级锁而不是 pid 文件，就是为了这一条。
#[test]
fn a_killed_instance_releases_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let (mut holder, ready) = spawn_holder(&dir);
    let held = single_instance::acquire(&dir).expect("第一次探测");
    assert!(
        matches!(held, SingleInstanceOutcome::HandedOver),
        "子进程该占着，实际是 {}",
        describe(&held)
    );

    holder.kill().expect("kill -9");
    holder.wait().expect("等子进程退出");
    let _ = std::fs::remove_file(&ready);

    // 锁文件还在磁盘上（那是标记，不是状态），但它已经不挡人了。
    assert!(
        Path::new(&dir).join("locks").join("gui.lock").exists(),
        "锁文件应当留在磁盘上——它不是状态，不需要清理"
    );
    let outcome = single_instance::acquire(&dir).expect("子进程死了之后应当能拿到");
    assert!(
        matches!(outcome, SingleInstanceOutcome::Primary(_)),
        "kill -9 之后新实例必须能起来，实际是 {}",
        describe(&outcome)
    );
}

/// 残留的 socket 文件（上一次崩了没来得及收）不该让下一个人起不来。
#[test]
fn a_stale_socket_is_replaced() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    let locks = dir.join("locks");
    std::fs::create_dir_all(&locks).unwrap();
    let paths = single_instance::Paths::for_data_dir(&dir);
    // 造一个「文件在、没人听」的 socket：绑一次再放开监听者就得到这个状态。
    {
        let listener = std::os::unix::net::UnixListener::bind(&paths.channel).unwrap();
        drop(listener);
    }
    assert!(paths.channel.exists(), "残留文件应当还在");

    let outcome = single_instance::acquire(&dir).expect("残留 socket 不该导致失败");
    assert!(
        matches!(outcome, SingleInstanceOutcome::Primary(_)),
        "残留 socket 应当被重建，实际是 {}",
        describe(&outcome)
    );
}

/// 锁被占、通道却没人在听：必须**报错**，绝不接管启动第二个。
///
/// 这是 ADR-0083 §4 表里最难取舍的那一行。接管看起来「体面」（用户总能打开
/// 窗口），但它把「每机每用户一个」这条不变式破了，而用户下次只会看到两个
/// 窗口，根本想不到是启动时的一次超时。
///
/// 这条同时把**锁**单独验了一遍：子进程不托管通道，所以让位那条路走不通，
/// 能拦住我们的只剩锁本身。
#[test]
fn a_silent_holder_makes_us_fail_instead_of_taking_over() {
    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let (mut holder, ready) = spawn_holder_with(&dir, true);
    let started = Instant::now();
    let outcome = single_instance::acquire(&dir);
    let elapsed = started.elapsed();
    let _ = std::fs::remove_file(&ready);
    let _ = holder.kill();
    let _ = holder.wait();

    let error = outcome.expect_err("锁被占且没人应答时必须报错，不许接管");
    assert_eq!(
        error.kind(),
        std::io::ErrorKind::TimedOut,
        "应当是超时，实际：{error}"
    );
    // 人话里要给出开关，否则用户被锁在门外时无路可走。
    let text = error.to_string();
    assert!(
        text.contains("QC_GUI_SINGLE_INSTANCE=0"),
        "报错要说清楚怎么脱身：{text}"
    );
    assert!(
        elapsed >= Duration::from_millis(900),
        "重试窗口盖住「刚拿锁还没 listen」那段，不能立刻放弃：{elapsed:?}"
    );
}

/// 收到激活请求之后，第一个实例必须把它变成 [`TrayCommand::ShowWindow`]。
///
/// 这条钉的是 ADR-0084 §3 那句「与托盘『显示主窗口』完全同一条路径」：只要
/// 这条映射成立，激活就自动继承了托盘那条路已经处理好的三端细节（原生调用
/// 推迟到 foreground executor、macOS 的 `deminiaturize:` 等等），不需要第二份。
#[test]
fn an_activate_request_becomes_a_show_window_command() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let SingleInstanceOutcome::Primary(primary) = single_instance::acquire(&dir).unwrap() else {
        panic!("干净的 data dir 应当能拿到锁");
    };
    let (tx, rx) = std::sync::mpsc::channel();
    primary.serve(tx);

    let paths = single_instance::Paths::for_data_dir(&dir);
    let mut stream = UnixStream::connect(&paths.channel).expect("连上刚起的通道");
    stream.write_all(b"{\"activate\":true}\n").unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    assert!(reply.contains("\"ok\":true"), "回复应当确认：{reply:?}");

    let command = rx
        .recv_timeout(Duration::from_secs(5))
        .expect("第一个实例应当收到一条命令");
    assert_eq!(command, qcode_gui::tray::TrayCommand::ShowWindow);
}

/// 不是激活请求的连接一律不许变成命令。
///
/// 这条通道在任何本机进程都能连（UDS 就在 data dir 里），所以「收到什么都不
/// 照做」必须成立：一条乱码不该把用户的窗口翻出来。
#[test]
fn stray_traffic_does_not_raise_the_window() {
    use std::io::Write;
    use std::os::unix::net::UnixStream;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let SingleInstanceOutcome::Primary(primary) = single_instance::acquire(&dir).unwrap() else {
        panic!("干净的 data dir 应当能拿到锁");
    };
    let (tx, rx) = std::sync::mpsc::channel();
    primary.serve(tx);

    let paths = single_instance::Paths::for_data_dir(&dir);
    for junk in [
        &b"{\"quit\":true}\n"[..],
        b"hello\n",
        b"{\"activate\":false}\n",
    ] {
        let mut stream = UnixStream::connect(&paths.channel).expect("连上通道");
        let _ = stream.write_all(junk);
    }
    assert!(
        rx.recv_timeout(Duration::from_millis(300)).is_err(),
        "非激活请求不该产生任何命令"
    );
}

fn describe(outcome: &SingleInstanceOutcome) -> String {
    match outcome {
        SingleInstanceOutcome::Primary(_) => "Primary".into(),
        SingleInstanceOutcome::HandedOver => "HandedOver".into(),
        SingleInstanceOutcome::Disabled(why) => format!("Disabled({why})"),
    }
}
