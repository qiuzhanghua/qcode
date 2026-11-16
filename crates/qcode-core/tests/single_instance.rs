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

use qcode_core::single_instance;
use qcode_core::single_instance::Outcome as SingleInstanceOutcome;

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
        let (tx, rx) = std::sync::mpsc::channel::<()>();
        primary.serve(move |request, reply| {
            let _ = qcode_core::single_instance::ack(request, reply);
            tx.send(()).is_ok()
        });
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

/// 通道收到 `activate` 之后必须把它交给调用方，并回复确认。
///
/// 把「请求 → 具体动作」那一步留给调用方是 ADR-0092 §11 搬家的结果：这个模块
/// 不再认识任何界面类型。GUI 那一侧映射到托盘「显示主窗口」是有意的
/// （ADR-0084 §3，见 `qcode-gui` 的 `activator`）：只要那条映射成立，激活就
/// 自动继承了托盘那条路已经处理好的三端细节（原生调用推迟到 foreground
/// executor、macOS 的 `deminiaturize:` 等等），不需要第二份。
#[test]
fn an_activate_request_reaches_the_handler() {
    use std::io::{BufRead, BufReader, Write};
    use std::os::unix::net::UnixStream;

    let tmp = tempfile::tempdir().unwrap();
    let dir = tmp.path().join("data");
    std::fs::create_dir_all(&dir).unwrap();

    let SingleInstanceOutcome::Primary(primary) = single_instance::acquire(&dir).unwrap() else {
        panic!("干净的 data dir 应当能拿到锁");
    };
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    primary.serve(move |request, reply| {
        let _ = qcode_core::single_instance::ack(request, reply);
        tx.send(()).is_ok()
    });

    let paths = single_instance::Paths::for_data_dir(&dir);
    let mut stream = UnixStream::connect(&paths.channel).expect("连上刚起的通道");
    stream.write_all(b"{\"activate\":true}\n").unwrap();
    let mut reply = String::new();
    BufReader::new(stream).read_line(&mut reply).unwrap();
    assert!(reply.contains("\"ok\":true"), "回复应当确认：{reply:?}");

    rx.recv_timeout(Duration::from_secs(5))
        .expect("第一个实例应当把请求交给调用方");
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
    let (tx, rx) = std::sync::mpsc::channel::<()>();
    primary.serve(move |request, reply| {
        let _ = qcode_core::single_instance::ack(request, reply);
        tx.send(()).is_ok()
    });

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

// ── `Stop` 的应答顺序（ADR-0092 §11 阶段 3）────────────────────────────────
//
// 这一组守的是一个真实修过的竞态：如果通道在回调里**先**取消时钟**再**写应答，
// 主循环可能在那两行之间 break、进程开始收摊，于是客户端永远收不到确认。
// 它会把「它听到了」误报成「它没听到」。

/// 请求能被解析成两个明确的动作，且歧义载荷被拒。
#[test]
fn the_wire_protocol_has_exactly_two_requests() {
    use qcode_core::single_instance::Request;

    assert_eq!(
        Request::parse("{\"activate\":true}\n"),
        Some(Request::Activate)
    );
    assert_eq!(Request::parse("{\"stop\":true}\n"), Some(Request::Stop));
    // 两个都是 true 的载荷只可能来自写错的客户端。猜它想要哪个的后果不对称——
    // 猜成 stop 会停掉用户的 daemon。
    assert_eq!(Request::parse("{\"activate\":true,\"stop\":true}"), None);
}

/// `Stop` 的应答必须在进程退出**之前**送达。
///
/// 造法：起一个真进程，它的回调先 `ack` 再返回 `false`（退出 accept 循环），然后
/// 立刻 `std::process::exit`。客户端必须拿到 `ok` 而不是连接断开。
///
/// 这条测的是**顺序契约**，不是某个实现细节：`serve` 把写入端交给回调，就是为了让
/// 这个顺序能由回调保证。
#[test]
fn a_stop_reply_is_written_before_the_handler_exits() {
    use qcode_core::single_instance::{Request, ack};

    let dir = tempfile::tempdir().expect("data dir");
    let data = dir.path().to_path_buf();

    let primary = match qcode_core::single_instance::acquire_in(
        qcode_core::single_instance::Scope::QCD,
        &data,
    )
    .expect("acquire")
    {
        qcode_core::single_instance::Outcome::Primary(primary) => primary,
        other => panic!("expected Primary, got {other:?}"),
    };

    primary.serve(move |request, reply| {
        // 关键顺序：ack 在前，「准备退出」在后。
        let _ = ack(request, reply);
        if request == Request::Stop {
            // 模拟「退出」：不再接活。真实实现里主循环随后就 break 了。
            return false;
        }
        true
    });

    // 客户端：必须读到 `ok`，而且不能是超时/断开。
    let stopped =
        qcode_core::single_instance::request_stop(qcode_core::single_instance::Scope::QCD, &data)
            .expect("the reply must arrive, not a broken pipe");
    assert!(
        stopped,
        "the handler acknowledged but the client saw nothing"
    );
}

/// 没人在听时 `request_stop` 返回 `Ok(false)`，不是一个「假的成功」。
#[test]
fn stopping_nothing_reports_false() {
    let dir = tempfile::tempdir().expect("data dir");
    // 不同目录 = 不同通道（`Scope` × 数据目录），所以这里一定没有监听者。
    let stopped = qcode_core::single_instance::request_stop(
        qcode_core::single_instance::Scope::QCD,
        dir.path(),
    )
    .expect("no listener is not an io error");
    assert!(!stopped, "nobody was listening, so nothing was stopped");
}

// ── 地址回退（ADR-0099 §2）────────────────────────────────────────────────
//
// 这一组守的是一个**静默失效**：`sun_path` 一超长，`bind` 报
// `AF_UNIX path too long`，于是 `acquire_in` 退回 `Disabled`——daemon 照常跑，只是
// 完全没有互斥，而警告写进了 detached 进程已经丢弃的 stderr。修复前这几条必然红。
//
// 造法：`tempfile` 在 macOS 上给出 `$TMPDIR`（`/var/folders/...`，约 49 字节），
// 再拼一段 120 字符的目录名，canonical 之后稳稳超过上限。

#[cfg(unix)]
fn too_long_data_dir(tmp: &Path) -> PathBuf {
    let deep = tmp.join("d".repeat(120));
    std::fs::create_dir_all(&deep).expect("mkdir deep");
    deep
}

/// 收掉一条（可能搬家过的）通道，连同它的私有 slot 目录。
///
/// 只有测试需要这个：真进程要么 `serve`（`accept_loop` 退出时自己收摊），要么被
/// `kill -9`（残留下一次 `listen` 会清）。而测试里「拿了 `Primary` 却不 `serve`」
/// 是常态，不收拾的话每跑一次就在 `/tmp` 里留一条 socket。
#[cfg(unix)]
fn tidy(paths: &single_instance::Paths) {
    let _ = std::fs::remove_file(&paths.channel);
    if let Some(dir) = paths.channel.parent() {
        let _ = std::fs::remove_dir(dir);
    }
}

/// 太长的 data dir 仍然必须**真的**抢到锁，而不是退回 `Disabled`。
#[cfg(unix)]
#[test]
fn a_too_long_data_dir_still_takes_the_lock() {
    let tmp = tempfile::tempdir().unwrap();
    let deep = too_long_data_dir(tmp.path());

    let paths = single_instance::Paths::for_data_dir(&deep);
    assert!(
        paths.channel_is_displaced().is_some(),
        "这条测试的前提是通道搬家；实际没搬（路径 {} 字节）",
        paths.channel.as_os_str().len()
    );
    // 锁**不**搬家：`locks/` 是「这个目录有没有驱动」的捷径。
    assert_eq!(paths.lock, paths.data_dir.join("locks").join("gui.lock"));

    let outcome = single_instance::acquire(&deep).expect("搬家不该报错");
    let text = describe(&outcome);
    tidy(&paths);
    assert!(
        matches!(outcome, SingleInstanceOutcome::Primary(_)),
        "太长的 data dir 必须照常互斥，实际是 {text}"
    );
}

/// 搬家之后的通道必须**真的能应答**——只验「拿到了锁」不够。
///
/// 第二个实例走的是「先连通道」那一步（ADR-0084 §2）：它能让位，就证明回退地址上
/// 有一个活的监听者。修复前这里会拿到 `Disabled`（`bind` 失败），而 `Disabled` 与
/// 「没人在跑」在用户眼里是同一件事。
#[cfg(unix)]
#[test]
fn a_displaced_channel_really_answers() {
    use qcode_core::single_instance::{Request, Scope, ack, request_stop};

    let tmp = tempfile::tempdir().unwrap();
    let deep = too_long_data_dir(tmp.path());

    let primary = match single_instance::acquire(&deep).expect("acquire") {
        SingleInstanceOutcome::Primary(primary) => primary,
        other => panic!("expected Primary, got {}", describe(&other)),
    };
    primary.serve(|request, reply| {
        let _ = ack(request, reply);
        request != Request::Stop
    });

    // 第二个实例让位 ⇒ 回退地址上有活监听者。
    let second = single_instance::acquire(&deep).expect("第二次");
    assert!(
        matches!(second, SingleInstanceOutcome::HandedOver),
        "第二个实例应当让位，实际是 {}",
        describe(&second)
    );

    // `Stop` 也走得通：这正是「够不着」会毁掉的那件事。
    let stopped = request_stop(Scope::GUI, &deep).expect("the reply must arrive");
    tidy(&single_instance::Paths::for_data_dir(&deep));
    assert!(stopped, "回退通道上的 Stop 没有被确认");
}

/// 回退目录必须是私有的：它收 `Stop` 请求。
///
/// 共享 `TMPDIR` 顶层放一条谁都能 `bind` 的 socket，别的本地用户就可以抢先占住
/// 这条路然后选择不理你（DoS），而且他能看见你的请求（ADR-0099 §2）。
#[cfg(unix)]
#[test]
fn the_fallback_directory_is_private() {
    use std::os::unix::fs::MetadataExt;

    let tmp = tempfile::tempdir().unwrap();
    let deep = too_long_data_dir(tmp.path());
    let paths = single_instance::Paths::for_data_dir(&deep);
    let dir = paths.channel.parent().expect("channel has a parent");

    // 先让目录被建出来（`acquire` 会建；这里直接建，免得依赖时序）。
    std::fs::create_dir_all(dir).unwrap();
    let meta = std::fs::metadata(dir).unwrap();
    assert_eq!(
        meta.mode() & 0o777,
        0o700,
        "回退目录必须是 0700，实际 {:o}（{}）",
        meta.mode() & 0o777,
        dir.display()
    );
    // SAFETY: `getuid` 无参数、无副作用。
    assert_eq!(
        meta.uid(),
        unsafe { libc::getuid() },
        "回退目录必须属于自己"
    );
    tidy(&paths);
}

/// 回退目录被占位时**拒绝使用**，而不是凑合着绑一条没人能连的通道。
///
/// 属主不对（别人拥有的同名目录）走的是同一条拒绝路径，但造不出那种环境，所以这条
/// 用「名字在、类型不对」来覆盖那个守卫。
///
/// 占位的是**本测试独有**的那一层（`<root>/qcode-<uid>/<该 data dir 的指纹>`），
/// 不是共享的 `qcode-<uid>`：后者会被并发跑的其它用例同时创建/删除，于是这条测试
/// 会变成一条随机红的测试（第一版就是这么红的——`write` 撞上别人刚建的同名目录）。
/// 用独有的路径既能污染**每一个**候选根目录，又不会碰到别人。
#[cfg(unix)]
#[test]
fn a_poisoned_fallback_directory_is_refused() {
    let tmp = tempfile::tempdir().unwrap();
    let deep = too_long_data_dir(tmp.path());
    let paths = single_instance::Paths::for_data_dir(&deep);
    let slot = paths.channel.parent().expect("channel has a parent");

    // 同一个指纹在**每个**候选根下都占位：回退是「按顺序试」，一个坏了还有下一个，
    // 只有全都不可用时才该退回 `Disabled`——那才是这条要验的状态。
    // SAFETY: `getuid` 无参数、无副作用。
    let ours = format!("qcode-{}", unsafe { libc::getuid() });
    let mut roots = vec![PathBuf::from("/tmp")];
    let tmp_root = std::env::temp_dir();
    if tmp_root.as_path() != Path::new("/tmp") {
        roots.push(tmp_root);
    }
    let slots: Vec<PathBuf> = roots
        .iter()
        .map(|root| root.join(&ours).join(slot.file_name().expect("slot name")))
        .collect();
    for each in &slots {
        let _ = std::fs::remove_dir_all(each);
        let parent = each.parent().expect("parent");
        // 上一次失败的运行可能把父目录也换成了文件（这条测试验的就是「类型不对」，
        // 所以它自己也得能收拾自己留下的那种状态）。
        if parent.is_file() {
            let _ = std::fs::remove_file(parent);
        }
        std::fs::create_dir_all(parent).expect("mkdir ours");
        std::fs::write(each, b"not a directory").expect("plant a file where the slot should be");
    }

    // 先取一次拿到指纹名，污染之后再取一次——搬家的原因要看**污染之后**的那次
    // 计算，否则断言的是旧结论。
    let displaced = single_instance::Paths::for_data_dir(&deep)
        .channel_is_displaced()
        .unwrap_or_default()
        .to_string();
    let outcome = single_instance::acquire(&deep).expect("不该 panic");
    let outcome_text = describe(&outcome);

    for each in &slots {
        let _ = std::fs::remove_file(each);
        // 收掉自己造的 slot 目录：它本来是「本测试独有」的一层，留着就是往用户的
        // /tmp 里扔垃圾。`remove_dir` 只在空的时候成功，所以不会误删别人的。
        if let Some(parent) = each.parent() {
            let _ = std::fs::remove_dir(parent);
        }
    }

    assert!(
        matches!(outcome, SingleInstanceOutcome::Disabled(_)),
        "全部回退目录被占位时必须让互斥失败，而不是绑一条假通道；实际是 {outcome_text}"
    );
    // 失败的原因分两层，两层都要在：`for_scope` 那一层说「造不出短目录」，`listen`
    // 那一层说「路径太长」。用户看日志先看到前者（`acquire_in` 的 warn），而
    // `Disabled` 里带的是后者。
    assert!(
        displaced.contains("cannot prepare"),
        "搬家的原因必须说清是回退目录不可用：{displaced}"
    );
    assert!(
        outcome_text.contains("shorter than SUN_LEN") || outcome_text.contains("too long"),
        "Disabled 必须带真实的 bind 失败原因：{outcome_text}"
    );
}
