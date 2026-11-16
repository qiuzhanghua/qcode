//! 把 `qcd` 从当前进程里分离出去（ADR-0092 §11 阶段 3）。
//!
//! 三平台各一段，但只有两个真实的差别：
//!
//! 1. **怎么让孩子活过我**。unix 用 `setsid` 让它成为新会话的首进程（没有控制终端、
//!    不接收 SIGHUP）；Windows 用 `DETACHED_PROCESS`。
//! 2. **怎么不让孩子拖住我这边的管道**。stdin 一律接到空设备，stdout/stderr 进日志
//!    或空设备——否则 `qc daemon start` 会一直等着孩子关掉它继承的那两个句柄。
//!
//! 这里**不做**双 fork（没必要：`setsid` 之后没有控制终端，也就没有 SIGHUP 可担），
//! 也**不做** pid 文件（ADR-0083 的结论：锁就是身份，pid 会被复用）。

use std::path::{Path, PathBuf};
use std::process::Command;

/// 分离启动 `qcd`，返回它的 pid（仅用于日志与给人看，不用于判「在不在跑」）。
///
/// `data_dir` 显式传给孩子，而不是让它去猜：`qc daemon start --data-dir X` 必须让
/// 那个 daemon 服务 X，而不是「默认目录里那个」。
pub fn spawn(
    program: &Path,
    data_dir: &Path,
    beat_secs: Option<u64>,
) -> Result<u32, std::io::Error> {
    let mut command = Command::new(program);
    command.arg("--data-dir").arg(data_dir);
    if let Some(secs) = beat_secs {
        command.arg("--beat").arg(secs.to_string());
    }

    // stdin 接空设备：daemon 不该继承调用者的终端，而一个继承来的 stdin 会让
    // 「谁在读我的管道」这个问题变得无解。
    command.stdin(std::process::Stdio::null());
    // stdout/stderr 丢掉：daemon 的现场是 `qc.log`（ADR-0082），不是调用者的终端。
    // 更要紧的是**不能继承**——`qc daemon start` 要立刻返回，而继承的管道会让它
    // 一直等孩子关句柄。
    command.stdout(std::process::Stdio::null());
    command.stderr(std::process::Stdio::null());

    detach(&mut command);
    let child = command.spawn()?;
    Ok(child.id())
}

/// unix：`setsid` 让孩子成为新会话的首进程。
///
/// 要的是它的两件事：**没有控制终端**（于是 Ctrl-C 与 SIGHUP 都到不了它），以及
/// **不随调用者的进程组一起被杀**。这也顺带说明为什么「活到 logout」在三个平台上
/// 不是同一件事（ADR-0092 §3）——`setsid` 只保证不因为终端消失而被 SIGHUP，而
/// systemd 的 `KillUserProcesses` 照样能收拾它。
#[cfg(unix)]
fn detach(command: &mut Command) {
    use std::os::unix::process::CommandExt as _;
    // SAFETY: `setsid` 是 async-signal-safe 的，且不碰任何内存。它在 fork 之后、
    // exec 之前于子进程中执行——正是 `pre_exec` 允许的那一类。
    unsafe {
        command.pre_exec(|| {
            // 已经在会话首领位置时 `setsid` 会失败（EPERM），而那不构成问题：
            // 目的已经达到。
            if libc::setsid() == -1 {
                let error = std::io::Error::last_os_error();
                if error.raw_os_error() != Some(libc::EPERM) {
                    return Err(error);
                }
            }
            Ok(())
        });
    }
}

/// Windows：`DETACHED_PROCESS` + `CREATE_NEW_PROCESS_GROUP`。
///
/// - `DETACHED_PROCESS`：不继承父进程的控制台。这正是我们要的——一个没有窗口的
///   常驻进程不该凭空多出一个控制台窗口。
/// - `CREATE_NEW_PROCESS_GROUP`：Ctrl-C 只送到调用者那一组，不波及 daemon。
///
/// 不做 Windows **服务**（ADR-0092 §12 非目标）：服务要在 SCM 里注册、要跑在
/// 独立会话里，而那是另一套语义（跨用户、开机就起）。
#[cfg(windows)]
fn detach(command: &mut Command) {
    use std::os::windows::process::CommandExt as _;
    use windows_sys::Win32::System::Threading::{CREATE_NEW_PROCESS_GROUP, DETACHED_PROCESS};
    command.creation_flags(DETACHED_PROCESS | CREATE_NEW_PROCESS_GROUP);
}

#[cfg(not(any(unix, windows)))]
fn detach(_command: &mut Command) {
    // 没有分离原语。`spawn` 照样能起进程，只是它绑在调用者身上——调用者退出它
    // 就跟着走。调用方（`qc daemon start`）会因此拿到一个「立刻就不在跑」的
    // daemon，而 `status` 会如实说没有。
}

/// 在 `PATH` 里找 `qcd`。
///
/// 这是 ADR-0092 §11 点名的诚实细节：**找不到必须说找不到**。一个说「已启动」但其实
/// 没启动的 `daemon start`，比没有这个子命令更糟——用户会以为定时任务有人管了。
///
/// 先看自己所在目录：`cargo run` / 解压到同一个 `dist/` 的场合，`qcd` 就在 `qc`
/// 旁边，而那个目录不一定在 `PATH` 里。这是发布物的实际布局（`build-for-release`
/// 把两个二进制放进同一个归档），不是猜测。
pub fn find_qcd() -> Result<PathBuf, NotInstalled> {
    let name = if cfg!(windows) { "qcd.exe" } else { "qcd" };

    // 1. 与自己同目录。
    if let Ok(me) = std::env::current_exe()
        && let Some(dir) = me.parent()
    {
        let sibling = dir.join(name);
        if sibling.is_file() {
            return Ok(sibling);
        }
    }

    // 2. `PATH`。手写而不是拉一个 which crate：只有这一处需要，而 `PATH` 的语义
    //    少到不值得一个依赖。
    if let Some(path) = std::env::var_os("PATH") {
        for dir in std::env::split_paths(&path) {
            let candidate = dir.join(name);
            if candidate.is_file() {
                return Ok(candidate);
            }
        }
    }

    Err(NotInstalled {
        looked_in: std::env::var_os("PATH")
            .map_or_else(Vec::new, |p| std::env::split_paths(&p).collect()),
    })
}

/// `qcd` 找不到。带着「找过哪些目录」出去，因为这才是用户能行动的信息。
#[derive(Debug)]
pub struct NotInstalled {
    looked_in: Vec<PathBuf>,
}

impl std::fmt::Display for NotInstalled {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        writeln!(f, "`qcd` was not found.")?;
        writeln!(f)?;
        writeln!(
            f,
            "The daemon is a separate binary. Install it next to `qc`, or put it on PATH:"
        )?;
        if self.looked_in.is_empty() {
            writeln!(f, "  (PATH is empty or unset)")?;
        } else {
            for dir in &self.looked_in {
                writeln!(f, "  {}", dir.display())?;
            }
        }
        write!(
            f,
            "\nTimed tasks will not fire until it exists. `qc schedule list` still works."
        )
    }
}

impl std::error::Error for NotInstalled {}

impl NotInstalled {
    /// 构造一条「找过这些目录」的消息，**供测试钉住文案形状**。
    ///
    /// 为什么要开这个口子：真正的「找不到 `qcd`」在 cargo 的测试环境里造不出来
    /// ——它总是把 `qc` 和 `qcd` 放进同一个 target 目录，于是 `find_qcd` 的第一
    /// 步（看兄弟文件）永远命中。而这一段文案是发布产物被拆开时用户的唯一指望，
    /// 所以它必须有一个能测的入口，而不是只能靠人读代码。
    pub fn describe_for_test(looked_in: &[String]) -> String {
        Self {
            looked_in: looked_in.iter().map(PathBuf::from).collect(),
        }
        .to_string()
    }
}
