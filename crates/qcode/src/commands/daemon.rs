//! `qc daemon start|stop|status`（ADR-0092 §11 阶段 3）。
//!
//! **只做「拉起 + 问话」**，自己不驱动。daemon 是独立二进制 `qcd`，因为把它塞进
//! `qc` 这个 package 会把 gpui → wgpu 链接进一个没有窗口的常驻进程（ADR-0092 §2），
//! 而那个后果不会报错，只在发布物体积上体现。
//!
//! 实现全部在 [`qcode_core::daemon`]：这里只负责**把答案变成终端上的一句话**。
//! 探测、拉起、停止都在那一处，因为 GUI 与 TUI 要问同一个问题（ADR-0098 §1）。
//!
//! ## 「在不在跑」怎么判
//!
//! 用**锁**，不用 pid 文件、不用「连一下试试」：
//!
//! - 抢到 `Scope::QCD` 的锁并立刻放开 ⇒ 没人在跑
//! - 抢不到 ⇒ 有人在跑，且**它有可能是另一个数据目录的**
//!
//! 这正是 ADR-0083 的结论（「锁就是身份，pid 会被复用」）在 daemon 上的复用，也是
//! ADR-0092 §5「一个用户一个 daemon」能被 `status` 如实回答的原因。

use std::fmt::Write as _;
use std::path::Path;

use super::*;
use qcode_core::daemon::Presence;

pub(crate) fn cmd_daemon(
    args: DaemonArgs,
    data_dir: Option<PathBuf>,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    // 用户级 base：与 `qcd` 里用的是同一个（默认数据目录），否则 `status` 会去问
    // 「这个数据目录有没有 daemon」——而问题问的是「这个用户有没有」。
    let user_dir = qcode_kernel::data_dir_hint(None);
    // 但这个 daemon 服务的目录可以是任意一个。
    let served = data_dir.unwrap_or_else(|| user_dir.clone());

    match args.command {
        DaemonCommand::Start { beat } => start(&served, beat),
        DaemonCommand::Stop => stop(&user_dir),
        DaemonCommand::Status { json } => status(&user_dir, &served, json),
    }
}

fn start(served: &Path, beat: Option<u64>) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let user_dir = qcode_kernel::data_dir_hint(None);

    // 先问，再起：抢锁不是为了自己用，而是为了不制造第二个 daemon。
    if let Presence::Running = qcode_core::daemon::presence(&user_dir) {
        // 已经有 daemon 了。ADR-0092 §5 是「一个用户一个」，所以这不是失败，
        // 但必须说清楚——用户以为刚起了一个，其实用的是老的。
        //
        // **不承诺报出它服务哪个目录**：锁只回答「有人」，服务目录那份信息只在
        // daemon 自己的进程里，没有任何可读位置。以前这句写着「`qc daemon status`
        // says which」，而 `status` 自己也说查不到——自相矛盾（ADR-0098 §3 修）。
        eprintln!(
            "qc daemon: already running (one daemon per user, ADR-0092 §5).\n\
             It may serve a different data directory; which one cannot be read from the lock."
        );
        return Ok(ExitCode::from(1));
    }

    // ADR-0092 §11 点名的诚实细节：找不到 `qcd` 就**明确失败**。一个说「已启动」
    // 但其实什么都没起的子命令，比没有它更糟。
    let program = match qcode_core::daemon::find_qcd() {
        Ok(path) => path,
        Err(error) => {
            eprintln!("qc daemon: {error}");
            return Ok(ExitCode::from(1));
        }
    };

    let pid = match qcode_core::daemon::spawn(&program, served, beat) {
        Ok(pid) => pid,
        Err(error) => {
            eprintln!("qc daemon: cannot start {}: {error}", program.display());
            return Ok(ExitCode::from(1));
        }
    };

    // 等它真的拿到锁，而不是「spawn 成功就说成功」。`spawn` 只证明 fork 成功——
    // 那个进程可能立刻因为驱动锁被占、配置坏了、或者数据目录不可写而退出。
    //
    // 两秒是实测的启动时间（`assemble` 要起 egress 代理、加载 skills，约 0.3s）；
    // 短于此的等待会把「慢」误报成「失败」，而失败是要人处理的。
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(3);
    // 锁机制本身不可用时（NFS、权限、`QC_QCD_SINGLE_INSTANCE=0`），`presence`
    // 永远答不出 `Running`。那不是「它没起来」，而是「问不出来」——两件事必须
    // 分开说，否则下面那句「probably exited」就成了一句假话：进程活得好好的，
    // 只是没有互斥。实测过（2026-11-16）：`QC_QCD_SINGLE_INSTANCE=0` 下 start 报
    // 「did not take the lock」，而 `ps` 里那个 qcd 正在跑。
    let mut blind_spot: Option<String> = None;
    while std::time::Instant::now() < deadline {
        match qcode_core::daemon::presence(&user_dir) {
            Presence::Running => {
                println!("qcd started (pid {pid}) — driving {}", served.display());
                println!(
                    "Logs: {}",
                    qcode_kernel::logging::current_log(served).display()
                );
                return Ok(ExitCode::SUCCESS);
            }
            Presence::Unknown(why) => {
                blind_spot = Some(why);
                break;
            }
            Presence::Absent => {}
        }
        std::thread::sleep(std::time::Duration::from_millis(50));
    }

    // 问不出来：说清是**判断不了**，不猜结局。
    if let Some(why) = blind_spot {
        eprintln!(
            "qc daemon: qcd was spawned (pid {pid}) but its state cannot be verified ({why}).\n\
             The lock mechanism is unavailable, so a running daemon would be invisible here.\n\
             It is most likely running; check `ps -p {pid}` or the log: {}",
            qcode_kernel::logging::current_log(served).display()
        );
        return Ok(ExitCode::from(1));
    }

    // spawn 成功但它没活下来。不说「已启动」——那正是 §11 要防的那句话。
    eprintln!(
        "qc daemon: qcd was spawned (pid {pid}) but did not take the driver lock within 3s.\n\
         It probably exited at startup; see {}",
        qcode_kernel::logging::current_log(served).display()
    );
    Ok(ExitCode::from(1))
}

fn stop(user_dir: &Path) -> Result<ExitCode, Box<dyn std::error::Error>> {
    match qcode_core::daemon::presence(user_dir) {
        Presence::Absent => {
            // 「没有在跑」是成功——`stop` 的意图已经满足。与 `qc daemon start`
            // 不同：那里「已经有了」是有害的（用户以为起了新的），这里没有。
            println!("qc daemon: not running.");
            Ok(ExitCode::SUCCESS)
        }
        Presence::Running => {
            // 走**控制通道**，不发信号。理由与 ADR-0083 拒绝 pid 文件同源：pid 会被
            // 复用，而「我拿到的这个 pid 就是当初那个进程」无法证明。通道还多给一样
            // 东西——应答：`kill` 成功只说明信号发出去了。
            match qcode_core::daemon::stop(user_dir) {
                Ok(true) => {
                    // 收到确认 **≠** 已经退出：它是在收到之后才开始收摊的。等它真
                    // 放开锁，否则「stop 之后立刻 status」会看到「还在跑」。
                    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(5);
                    while std::time::Instant::now() < deadline {
                        if let Presence::Absent = qcode_core::daemon::presence(user_dir) {
                            println!("qc daemon: stopped.");
                            return Ok(ExitCode::SUCCESS);
                        }
                        std::thread::sleep(std::time::Duration::from_millis(50));
                    }
                    // 确认了但没停：说清楚，不要报成功。这可能是一次卡住的回合
                    // （它不该无限期挡着退出，但阶段 4 才处理在跑的回合）。
                    eprintln!(
                        "qc daemon: it acknowledged the stop but is still running after 5s.\n\
                         It may be finishing a turn; check `qc daemon status` again."
                    );
                    Ok(ExitCode::from(1))
                }
                Ok(false) => {
                    // 锁在，但通道连不上：半死的实例（挂着但没在监听）。这种情形
                    // 必须与「没有实例」分开——刚探测到它在跑。
                    eprintln!(
                        "qc daemon: the lock is held but nothing answers on the control channel.\n\
                         That process is wedged or shutting down. Wait, or kill it by pid."
                    );
                    Ok(ExitCode::from(1))
                }
                Err(error) => {
                    eprintln!("qc daemon: cannot send the stop request: {error}");
                    Ok(ExitCode::from(1))
                }
            }
        }
        Presence::Unknown(why) => {
            eprintln!("qc daemon: cannot tell whether it is running ({why}).");
            Ok(ExitCode::from(1))
        }
    }
}

fn status(
    user_dir: &Path,
    served: &Path,
    json: bool,
) -> Result<ExitCode, Box<dyn std::error::Error>> {
    let running = qcode_core::daemon::presence(user_dir);
    // 数据目录级的驱动状态：它回答「这个目录的时钟是谁在推」，与那把用户级锁是两回事
    // （ADR-0091 与 §5 各一把）。
    let store = SessionStore::new(served.to_path_buf());
    let driver =
        qcode_core::scheduler::DriverStatus::read(&store, qcode_core::schedule::now_local());

    if json {
        let daemon = match &running {
            Presence::Absent => serde_json::json!({ "state": "stopped" }),
            Presence::Running => serde_json::json!({ "state": "running" }),
            Presence::Unknown(why) => serde_json::json!({ "state": "unknown", "why": why }),
        };
        let report = serde_json::json!({
            "daemon": daemon,
            "data_dir": served.display().to_string(),
            "driver": driver_json(driver),
        });
        println!("{report}");
        return Ok(ExitCode::SUCCESS);
    }

    let mut report = String::new();
    match &running {
        Presence::Absent => report.push_str("daemon：没有在跑\n"),
        Presence::Running => report.push_str("daemon：**在跑**（一个用户一个）\n"),
        Presence::Unknown(why) => {
            report.push_str(&format!("daemon：**无法判断**（{why}）\n"));
        }
    }
    let _ = writeln!(report, "数据目录：{}", served.display());
    // 这一行来自 ADR-0091 的四态：它说「这个目录的时钟」而不只是「daemon」。
    let _ = writeln!(report, "驱动：{}", driver.describe());
    if let Presence::Running = running {
        report.push_str("    （它可能服务**另一个**数据目录；锁只回答「有人」，不回答「是谁」）\n");
    }
    write_report(&report)?;
    Ok(ExitCode::SUCCESS)
}

/// 与 `commands/schedule.rs` 里同名函数的形状一致；两处都只是把四态映射成 JSON。
fn driver_json(driver: qcode_core::scheduler::DriverStatus) -> serde_json::Value {
    use qcode_core::scheduler::DriverStatus;
    let state = match driver {
        DriverStatus::Running { .. } => "running",
        DriverStatus::Held { .. } => "held",
        DriverStatus::Gone { .. } => "gone",
        DriverStatus::Unknown => "unknown",
    };
    let at = match driver {
        DriverStatus::Running { at } | DriverStatus::Held { at: Some(at) } => Some(at.to_string()),
        DriverStatus::Held { at: None } | DriverStatus::Gone { .. } | DriverStatus::Unknown => None,
    };
    serde_json::json!({ "state": state, "at": at })
}
