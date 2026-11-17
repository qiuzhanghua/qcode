//! `qcd` 的前台形态（ADR-0092 §11 阶段 2）。
//!
//! 只做「跑着」这一件事：装配引擎、抢单实例锁、每 30 秒推一次时钟，Ctrl-C 停。
//! 分离进程、`qc daemon start|stop|status`、三平台各一段，都在阶段 3——先把「不开
//! 任何 GUI 也能跑完一条任务」验出来，那是 ADR-0092 唯一的核心验收。

use std::path::PathBuf;
use std::time::{Duration, Instant};

use qcd::Daemon;
use qcode_core::single_instance::{Outcome, Scope};

/// `qcd --help` 的正文。
///
/// 与 `--version` 一样必须在任何初始化之前打印，理由见 [`early_exit`]。
const HELP: &str = "\
qcd — 定时任务的常驻驱动（ADR-0092）

Usage: qcd [--beat <secs>] [--data-dir <path>]
       qcd --version | --help

一个用户只有一个 qcd（ADR-0092 §5）：它的互斥锁与 --data-dir 无关，
所以同一个用户的两个不同数据目录的 qcd 会互相拒绝。换一个 OS 用户另起
一个是允许的——锁的 base 就在那个用户自己的家目录下。
";

/// `--version` / `--help`：在日志、`lifecycle`、那把锁**之前**回答。
///
/// # 为什么位置是这个函数唯一的难点
///
/// 这两行由脚本调用：`build-for-release` 拿产物的 `--version` 与文件名对账，
/// `qc update` 装完 `qcd` 也读一次（`update::verify_daemon`）。而它们排在初始化
/// 之后有一处**真实**的副作用——实测 `qcd --help`（这一版之前）会先在数据目录里
/// 建出 `logs/`，再写一对 `session starting` / `session ending`。一个只问问题的
/// 调用不该留下任何现场；`qc --version` 就没有这个问题（clap 在解析阶段就退出）。
///
/// 第二个约束：必须早于 `acquire_in`。否则「磁盘上这个 qcd 是哪个版本」在
/// **正跑着一个 qcd 时**就问不出来——而那恰恰是最常问的时候（`qc update` 刚换完
/// 文件、老进程还活着）。版本问答是只读的，它不该被一把表示「谁在跑」的锁挡住。
///
/// 只认大写的 `-V`：`-v` 留给将来的 verbose，别让它先在版本上被占掉。
fn early_exit() -> Option<std::process::ExitCode> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            // 有取值的开关：连它的值一起跳过。否则 `qcd --data-dir --version`
            // 里那个 `--version` 会被当成请求，而它其实是一个（很怪的）目录名。
            "--data-dir" | "--beat" => index += 2,
            "--version" | "-V" => {
                // 与 `qc <version>` 同形。不是审美：`build-for-release` 用
                // `awk '{print $NF}'` 取版本，两边同形才能共用一条对账逻辑。
                println!("qcd {}", env!("CARGO_PKG_VERSION"));
                return Some(std::process::ExitCode::SUCCESS);
            }
            "--help" | "-h" => {
                println!("{HELP}");
                return Some(std::process::ExitCode::SUCCESS);
            }
            _ => index += 1,
        }
    }
    None
}

#[tokio::main]
async fn main() -> std::process::ExitCode {
    // 最先做的事，先于一切副作用（见 [`early_exit`]）。
    if let Some(exit) = early_exit() {
        return exit;
    }

    // 先解析参数再初始化日志：`--data-dir` 决定日志落在哪里，而 daemon 的日志是它
    // 唯一的现场（ADR-0082）——写错目录等于没有日志。
    let data_dir = match data_dir_arg() {
        Ok(dir) => dir,
        Err(error) => {
            eprintln!("qcd: {error}");
            return std::process::ExitCode::from(2);
        }
    };

    // **必须**在第一条 `tracing` 之前装好 subscriber，否则什么都不会落盘。`qcd`
    // 没有终端，一行都不写等于这个进程从未存在过——这正是第一版实跑时的样子：
    // 启动、退出、`qc.log` 里一个字都没有。
    qcode_kernel::logging::init(None);
    qcode_kernel::logging::attach_file(&data_dir);

    qcode_core::lifecycle::install_reaper(qcode_tools::child_groups::kill_all);
    qcode_core::lifecycle::start("qcd", &data_dir);

    let exit = match run().await {
        Ok(()) => 0,
        Err(error) => {
            // 无终端：说出来给日志，同时给 stderr（前台跑时有人看得见）。
            tracing::error!(%error, "qcd stopped with an error");
            eprintln!("qcd: {error}");
            1
        }
    };
    qcode_core::lifecycle::finish(exit);
    std::process::ExitCode::from(exit as u8)
}

/// 只挑出 `--data-dir`，因为日志初始化必须发生在完整参数校验之前。
///
/// 重复一次扫描不算问题：`run()` 里那次才是权威的（它会拒绝未知参数），这里只
/// 需要知道「日志该写哪」。
fn data_dir_arg() -> Result<PathBuf, String> {
    let args: Vec<String> = std::env::args().skip(1).collect();
    let mut index = 0;
    while index < args.len() {
        if args[index] == "--data-dir" {
            let value = args
                .get(index + 1)
                .ok_or_else(|| "--data-dir needs a value".to_string())?;
            return Ok(PathBuf::from(value));
        }
        index += 1;
    }
    Ok(qcode_kernel::data_dir_hint(None))
}

/// 等驱动锁空出来，然后拿下它（ADR-0092 §10：daemon 让路）。
///
/// 为什么要等而不是退出：先开窗口、后启 daemon 是很自然的顺序，而「因为你开着窗口
/// 所以 daemon 直接不启动」要求人在关窗口之后再手敲一次 `qc daemon start`。等的代价
/// 是一个睡着的进程，收益是那个顺序不必有人值守。
///
/// 等待期间**什么都不做**：不写心跳（那会让 `qc schedule list` 报告有两个驱动）、
/// 不扫任务表。就是每秒试一次锁。
///
/// 注意这把锁是**数据目录级**的（ADR-0091），所以「让路」的范围就是那个目录：另一个
/// 数据目录的 GUI 开着不影响这里。
fn wait_for_driver_lock(
    store: &qcode_core::store::SessionStore,
    identity: &str,
    sink: &mut dyn qcode_driver::events::Sink,
    stop: &tokio_util::sync::CancellationToken,
) -> Result<Option<qcode_core::store::FileLock>, Box<dyn std::error::Error>> {
    use qcode_driver::events::Event;
    let mut announced = false;
    loop {
        // **等待必须是可打断的**（ADR-0100 实测抓到）。没有这一句的话：GUI 正驱动
        // 时（`WindowDriven`）点按钮起的 qcd 会在这里排队，然后 `qc daemon stop`
        // 或再点一次按钮**都杀不掉它**——控制通道的应答已经写了、`cancel()` 也调
        // 过了，但主线程卡在这个同步循环里，永远走不到那个 `select!`。进程留着，
        // 锁已经放掉，于是用户在界面上看到的是「灰的、但 `ps` 里有个 qcd」。
        //
        // 放在循环顶部而不是 sleep 之后：点击与这一拍之间不该再有 1 秒的窗口。
        if stop.is_cancelled() {
            sink.emit(Event::Driving {
                at: qcode_core::schedule::now_local(),
                identity: identity.to_string(),
                driving: false,
            });
            eprintln!("qcd: stopping (it was waiting for the other driver)");
            return Ok(None);
        }
        match store.try_lock_file(qcode_core::scheduler::DRIVER_LOCK) {
            Ok(Some(lock)) => {
                // 「我开始开车」写进可跟随的事件流，于是「谁在开车」有时间线——
                // 排「为什么那段时间没触发」时，这条是第一眼看的东西。
                sink.emit(Event::Driving {
                    at: qcode_core::schedule::now_local(),
                    identity: identity.to_string(),
                    driving: true,
                });
                if announced {
                    // 说一声「接上了」：否则日志里只有「让路」那一行，看起来像
                    // 一直没在干活。
                    tracing::info!("the other driver is gone; taking over");
                    eprintln!("qcd: taking over (the other driver exited)");
                }
                return Ok(Some(lock));
            }
            Ok(None) => {
                if !announced {
                    // 让路也是事件：一个「daemon 在等 GUI 退出」的时间段，正是
                    // 「为什么那几小时没触发」的答案。
                    sink.emit(Event::Driving {
                        at: qcode_core::schedule::now_local(),
                        identity: identity.to_string(),
                        driving: false,
                    });
                    // 只喊一次。每秒喊一次会把日志刷满，而这件事没变过。
                    tracing::info!(
                        "another process drives this data directory; waiting (ADR-0092 §10)"
                    );
                    eprintln!(
                        "qcd: another driver (a GUI window?) holds this data directory; \
                         waiting for it to exit. Ctrl-C to give up."
                    );
                    announced = true;
                }
                std::thread::sleep(std::time::Duration::from_secs(1));
            }
            Err(error) => return Err(format!("cannot probe the driver lock: {error}").into()),
        }
    }
}

async fn run() -> Result<(), Box<dyn std::error::Error>> {
    // `--help` / `--version` 已经在 `main` 最前面答完了（[`early_exit`]），这里
    // 只剩真正要起一个 daemon 的那些参数。
    let args = std::env::args().skip(1).collect::<Vec<_>>();
    let mut beat = qcd::DEFAULT_BEAT;
    let mut data_dir: Option<PathBuf> = None;
    let mut index = 0;
    while index < args.len() {
        match args[index].as_str() {
            "--beat" => {
                let value = args.get(index + 1).ok_or("--beat needs a value")?;
                let secs: u64 = value.parse().map_err(|_| "--beat must be seconds")?;
                beat = Duration::from_secs(secs.max(1));
                index += 2;
            }
            "--data-dir" => {
                let value = args.get(index + 1).ok_or("--data-dir needs a value")?;
                data_dir = Some(PathBuf::from(value));
                index += 2;
            }
            // 不带 `qcd:` 前缀：`main` 打印时已经加了一次，写在这里会得到
            // `qcd: qcd: unknown argument "-v"`。
            other => return Err(format!("unknown argument {other:?}").into()),
        }
    }

    // ── 1. 用户级单实例（ADR-0092 §5）────────────────────────────────────
    //
    // base 是**默认**数据目录，不是 `--data-dir` 指的那个：这把锁要回答的是「这个
    // 用户是不是已经有一个 qcd」，而用 `--data-dir` 就会变成「这个数据目录有没有
    // qcd」——于是两个不同目录的 daemon 各自「成功」，却违反了「一个用户一个」。
    //
    // 作用域是**每个用户**，不是每台机器：base 在家目录下，所以两个 OS 用户各起
    // 一个是合法的。措辞见 ADR-0098 §2——那里有实测记录。
    //
    // 传错的后果不会报错，所以这段话必须留在这里。
    let user_dir = qcode_kernel::data_dir_hint(None);
    // 「请退出」的信号。由控制通道的 `Stop` 触发，主循环 select 在它上面。
    //
    // 用 `CancellationToken` 而不是 `bool` 标志：标志要等下一拍才被看见（默认
    // 30 秒），而 `stop` 该是立刻的。
    let stop = tokio_util::sync::CancellationToken::new();
    // 拿 `Option` 是因为 `Disabled` 分支没有锁可持有。`Some` 的 `Primary` 必须活到
    // 进程结束——掉了它就等于放锁（`Primary` 的文档就是这么要求的）。
    // `serve_primary` 把锁**移进监听线程**：不 `serve` 的话锁只是「占着」而通道
    // 没人应答，于是 `qc daemon status` 会看到「有锁但连不上」——那被正确地报成
    // 「无法判断」。daemon 必须应答，否则它对自己的存在只能给出一句含糊的话。
    // 抢到的 `Primary` 全部交给 `serve`（锁随监听线程活到进程结束），所以这里
    // 不需要再保存它。`Disabled` 分支没有锁可交，行为一样。
    match qcode_core::single_instance::acquire_in(Scope::QCD, &user_dir)
        .map_err(|error| format!("cannot take the user-wide lock: {error}"))?
    {
        Outcome::Primary(primary) => {
            // 应答通道：`qc daemon status` 靠它确认「有活实例」，`qc daemon stop`
            // 靠它把「请退出」送到这里。daemon 没有前台可「切到」，所以激活请求
            // 只是记一句日志。
            //
            // `Stop` 通过 `CancellationToken` 交给主循环：**不用信号**是因为
            // pid 不在锁里（ADR-0083），而通道比信号多给一样东西——应答。
            let stop_for_channel = stop.clone();
            primary.serve(move |request, reply| match request {
                qcode_core::single_instance::Request::Activate => {
                    tracing::debug!("activation request; qcd has no window to raise");
                    // 应答由这里写（`serve` 的契约），不是由 accept_loop 代写。
                    let _ = qcode_core::single_instance::ack(request, reply);
                    true
                }
                qcode_core::single_instance::Request::Stop => {
                    // **顺序是这段代码的全部意义**：先确认，再退出。
                    //
                    // 反过来写过的版本有个真实竞态：`cancel()` 之后主循环立刻
                    // break，进程开始收摊，这个线程手里的应答就可能永远发不出去
                    // ——客户端于是收到连接断开，分不清「它没听到」与「它听到了
                    // 但来不及说」。停一个进程的人必须拿到确认。
                    let _ = qcode_core::single_instance::ack(request, reply);
                    tracing::info!("stop requested over the control channel");
                    stop_for_channel.cancel();
                    // 不再接活：通道随线程结束一起关掉。
                    false
                }
            });
        }
        Outcome::HandedOver => {
            // 明确失败，不假装成功（ADR-0092 §11）：一个说「已启动」但其实没启动的
            // 进程，比没有这个能力更糟。
            eprintln!(
                "qcd: another qcd is already running for this user.\n\
                 One daemon per user (ADR-0092 §5); stopping this one."
            );
            return Err("already running".into());
        }
        Outcome::Disabled(why) => {
            // 锁机制不可用时**继续跑**：单实例是「防重复」而不是「防启动」，而定时
            // 任务不跑是更坏的后果。说一句，别静默。
            eprintln!("qcd: single-instance check skipped ({why}); two daemons could overlap");
        }
    };

    // ── 2. 装配引擎（ADR-0036：唯一装配点）────────────────────────────────
    let assembled = qcode_kernel::assemble(
        None,
        &std::env::current_dir()?,
        data_dir,
        None,
        Default::default(),
    )
    .await?;

    // ── 3. 驱动锁（ADR-0091，数据目录级）──────────────────────────────────
    //
    // 与上面那把是两回事：这把回答「谁是**这个数据目录**的时钟」。
    //
    // ADR-0092 §10：GUI 已经在推的时候 daemon **让路**——不抢、等它退出、然后接手。
    // 不是退出进程，因为那会让「先开窗口、后启 daemon」这个很自然的顺序需要人再手动
    // 起一次。
    //
    // 等待期间**不做任何事**：不写心跳（那会让 `qc schedule list` 说「有两个驱动」）、
    // 不扫任务表（扫了也不会投，而且会读盘）。就是纯等。
    // `_driver_lock` 必须活到 `run()` 结束：掉了它就等于放锁。
    // 出口先建：让路/接手也是事件，它们发生在「什么时候拿到锁」之前。
    let mut sink = qcode_driver::events::FileSink::under(assembled.store.root());
    let sink_path = sink.path().to_path_buf();
    // 身份要在这里算：它写进事件，而事件在拿锁的过程中就开始了。
    let identity = format!(
        "daemon:{}",
        qcode_core::schedule::now_local()
            .to_string()
            .replace(' ', "T")
    );
    // `None` = 还在等驱动锁的时候被叫停了。那不是失败：用户就是让它停的。
    let Some(_driver_lock) = wait_for_driver_lock(&assembled.store, &identity, &mut sink, &stop)?
    else {
        return Ok(());
    };

    // ── 4. 跑着 ────────────────────────────────────────────────────────────
    // 可跟随的事件出口（ADR-0092 §8）：一行一条 JSON，`tail -f` 就能看着它跑。
    // 本期只留缝，GUI 的跟随界面是后续的事。
    let mut daemon = Daemon::new(assembled, beat).with_sink(Box::new(sink));
    tracing::info!(path = %sink_path.display(), "schedule events go here");
    eprintln!(
        "qcd: driving {} (beat {}s, identity {})",
        daemon.assembled.store.root().display(),
        daemon.beat().as_secs(),
        daemon.identity()
    );

    let mut clock = tokio::time::interval(beat);
    // 第一拍立刻到来（`interval` 的默认行为），那正是我们要的：进程起来就该扫一次，
    // 而不是先干等 30 秒。
    //
    // 每一拍传 `Instant::now()` 而不是累加 `beat`：`Clock` 拿它算「距上一拍多久」，
    // 而机器休眠时 tokio 的定时器与单调时钟会一起走过那段时间——用真实值才看得出
    // 「进程当时不在」（ADR-0059 §2）。
    loop {
        tokio::select! {
            _ = clock.tick() => {
                let report = daemon.tick(Instant::now()).await;
                if report.changed_tasks() {
                    tracing::info!(
                        fired = report.fired,
                        skipped = report.skipped,
                        orphaned = report.orphaned,
                        recovered = report.recovered,
                        confirmed = report.confirmed,
                        "schedule beat"
                    );
                }
            }
            // Ctrl-C：干净退出。正在跑的回合随进程结束而中断，会话日志里留下
            // `interrupted`（`repair_interrupted` 下次补）。
            // 控制通道来的「请退出」（`qc daemon stop`）。与 Ctrl-C 同一条退出
            // 路径：跑完当前这一拍就走。
            _ = stop.cancelled() => {
                eprintln!("qcd: stopping (requested over the control channel)");
                break;
            }
            _ = tokio::signal::ctrl_c() => {
                eprintln!("qcd: stopping");
                break;
            }
        }
    }
    Ok(())
}
