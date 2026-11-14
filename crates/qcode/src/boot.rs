//! 启动形态决定前端（ADR-0086）。
//!
//! `qc` 只有一个可执行文件，也没有 `gui` 子命令：敲 `qc` 得到什么，取决于**它是怎么
//! 被拉起来的**。终端里敲 = TUI，点图标 / 双击 = 窗口。判定分三层，越靠前越可信：
//!
//! | 层 | 依据 | 强度 |
//! |---|---|---|
//! | 1 | 入口自己传的内部标记 [`LAUNCHED_FROM_GUI`] | 事实，不是推断 |
//! | 2 | 平台信号（见各端 `platform` 模块） | 只能证伪「终端」 |
//! | 3 | 都没有 | TUI |
//!
//! # 为什么第 1 层是首选
//!
//! `.app` 的 `CFBundleExecutable`、`.desktop` 的 `Exec=`、`.lnk` 的 target 都是
//! 我们自己生成的东西，加一个参数是零成本的确定性做法。只靠第 2 层会在真实场景里
//! 判错：从终端敲 `open -a QCode.app` 起来的进程带着终端的 `TERM`（实测），而从
//! Finder 起的一次连 `PATH` 都没有。
//!
//! # 判错的方向是不对称的
//!
//! 两层都只在拿到**正面证据**时才走 GUI。终端用户被弹出窗口会毁掉脚本与管道
//! （`qc | jq` 立刻不可用），而图形用户误得 TUI 只是「点了没反应」。所以第 2 层要求
//! 两个独立信号同时成立，宁可漏判。

/// 入口传给我们的内部标记。
///
/// 用户看不见（不进 `--help`），但可以在终端手动用它强迫走 GUI 分支——调试窗口时
/// 这是唯一不必先打包的路。
pub const LAUNCHED_FROM_GUI: &str = "--launched-from-gui";

/// `Info.plist` 的 `LSEnvironment` 挂进来的标记（ADR-0086 §4）。
///
/// 内部参数 [`LAUNCHED_FROM_GUI`] 是给能改命令行的入口用的（`.desktop` 的
/// `Exec=`、`.lnk` 的 target、开发者手敲）。macOS 的 `.app` 没有命令行可改——
/// 双击时系统按 `Info.plist` 直接 exec `CFBundleExecutable`——所以那一条路上
/// 挂的是这个环境变量。两条路都是「我们自己的入口显式说一声」，强度一样。
pub const GUI_MARKER_ENV: &str = "QCODE_LAUNCHED_FROM_GUI";

/// 这次该开哪种前端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    /// 终端界面（默认）。
    Tui,
    /// 桌面窗口。
    Gui,
}

impl Frontend {
    /// 日志与 `command_name` 用的短名。
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Tui => "tui",
            Self::Gui => "gui",
        }
    }
}

/// 判定结果，连同**依据**一起返回。
///
/// 依据必须一起出来：这是唯一能在事后解释「为什么这次开了窗口」的东西，而形态判定
/// 恰恰是最难复现的一类问题（换个启动方式就换个答案）。
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct Decision {
    pub frontend: Frontend,
    /// 一句话说明判定依据，进日志。取值是稳定的短标识，便于 grep。
    pub reason: &'static str,
}

/// 拿掉内部标记，返回「剩下的参数」与「是否带了标记」。
///
/// 必须在 clap 之前调用：标记不是给用户看的选项，让 clap 收下它等于把它放进
/// `--help` 与补全里。
pub fn strip_launch_marker(args: impl IntoIterator<Item = String>) -> (Vec<String>, bool) {
    let mut rest = Vec::new();
    let mut marked = false;
    for arg in args {
        if arg == LAUNCHED_FROM_GUI {
            marked = true;
        } else {
            rest.push(arg);
        }
    }
    (rest, marked)
}

/// 三层判定。
///
/// `marked` 来自 [`strip_launch_marker`]。第 2 层只在没标记时才有意义——标记是事实，
/// 平台信号是推断。
pub fn detect(marked: bool) -> Decision {
    // 环境变量那条与命令行标记等价：都来自我们自己的入口。
    let env_marker = std::env::var_os(GUI_MARKER_ENV).is_some();
    detect_with(marked, env_marker)
}

/// 判定主体，**不读环境**。
///
/// 环境读取留在这里之外，是为了能测：`std::env` 是进程级的，而 `cargo test` 的用例
/// 是并发线程。第一版直接在这样的函数里读环境变量，于是「设定标记」和「清除标记」
/// 两条用例互相看到对方的值，随机红——这条注释就是那次失败的墓志铭。
fn detect_with(marked: bool, env_marker: bool) -> Decision {
    if marked || env_marker {
        return Decision {
            frontend: Frontend::Gui,
            reason: "launch-marker",
        };
    }
    match platform::graphical_session() {
        Some(signal) => Decision {
            frontend: Frontend::Gui,
            reason: signal,
        },
        None => Decision {
            frontend: Frontend::Tui,
            reason: "no-gui-signal",
        },
    }
}

#[cfg(target_os = "macos")]
mod platform {
    /// GUI 会话的两个独立信号，**必须同时成立**。
    ///
    /// 实测（本机，一个只自 dump 环境与父进程的最小 `.app`）：
    ///
    /// | 启动方式 | 父进程 | `__CFBundleIdentifier` |
    /// |---|---|---|
    /// | 终端直接 exec | `/bin/zsh` | 终端自己的（`com.mitchellh.ghostty`） |
    /// | `env -i open -a`（= 双击） | `launchd`（pid 1） | 我们自己的 |
    ///
    /// 单独看任意一个都会误判：`setsid` 起的进程父进程也不是 shell，
    /// 而从终端 `open -a` 时 `__CFBundleIdentifier` 已经是我们自己了（父进程也是
    /// `launchd`）——那种情况**确实**是图形启动，判成 GUI 是对的。
    pub(super) fn graphical_session() -> Option<&'static str> {
        // `TERM` 完全不可用于此：`open` 会把调用者的环境传进来，实测从终端
        // `open -a` 起的进程带着 `TERM=xterm-ghostty`。
        if std::env::var_os("TERM").is_some() {
            return None;
        }
        if std::env::var_os("TERM_PROGRAM").is_some() {
            return None;
        }
        // 有终端设备就说明是终端里启动的：这条比父进程名可靠（父进程可能是任何东西）。
        if stdin_is_tty() {
            return None;
        }
        if parent_is_launchd() && bundle_identifier_is_ours() {
            return Some("coder-macos-launchd");
        }
        None
    }

    fn stdin_is_tty() -> bool {
        // SAFETY: `isatty` 只读一个 fd，对任意整数都安全。
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }

    /// 父进程是不是 `launchd`（pid 1）。
    ///
    /// 用 `getppid` 而不是 `ps`：受限环境里 `/bin/ps` 会被拒（`Operation not
    /// permitted`），而这是每次启动都要跑的一条判定。
    fn parent_is_launchd() -> bool {
        // SAFETY: `getppid` 无参数、无副作用。
        unsafe { libc::getppid() == 1 }
    }

    /// `__CFBundleIdentifier` 是不是我们自己的。
    ///
    /// 终端继承来的那个会是终端 app 的 id。我们只在**同时**是 launchd 子进程时
    /// 才看这个，所以这里只需要排除「明显不是我们」的情形。
    fn bundle_identifier_is_ours() -> bool {
        match std::env::var("__CFBundleIdentifier") {
            Ok(id) => id.starts_with("com.qcode."),
            // 没有这个变量（裸二进制直接 exec，不经 .app）：不是图形会话的证据。
            Err(_) => false,
        }
    }
}

#[cfg(target_os = "linux")]
mod platform {
    /// Linux 上的两个独立信号：没有终端设备，且处在图形会话里。
    pub(super) fn graphical_session() -> Option<&'static str> {
        if stdin_is_tty() {
            return None;
        }
        // `.desktop` 里我们写 `Terminal=false`，于是这条进程没有控制终端。
        // 但「没有 tty」也可能是 systemd unit / cron / CI，所以还要图形会话的证据。
        let session = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let graphical = matches!(session.as_str(), "wayland" | "x11" | "mir");
        if !graphical {
            return None;
        }
        // 没有 DISPLAY / WAYLAND_DISPLAY 就打不开窗口，别把人送进一条必然失败的
        // 路——那比留他在 TUI 里更糟。
        if std::env::var_os("WAYLAND_DISPLAY").is_none() && std::env::var_os("DISPLAY").is_none() {
            return None;
        }
        Some("coder-linux-desktop")
    }

    fn stdin_is_tty() -> bool {
        // SAFETY: `isatty` 只读一个 fd，对任意整数都安全。
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }
}

#[cfg(windows)]
mod platform {
    /// Windows 上的两个独立信号：没有控制台窗口，且父进程是资源管理器。
    pub(super) fn graphical_session() -> Option<&'static str> {
        if has_console() {
            return None;
        }
        if parent_is_explorer() {
            return Some("coder-windows-explorer");
        }
        None
    }

    /// 有没有控制台。`qc` 是控制台子系统程序，从 `cmd.exe` / `pwsh` / Windows
    /// Terminal 启动时一定挂着控制台；双击时 Windows 会给它新建一个**自己的**
    /// 控制台——那种情形要靠父进程区分（下面）。
    fn has_console() -> bool {
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        // SAFETY: 无参数、无副作用；返回 0 表示没有控制台。
        !unsafe { GetConsoleWindow() }.is_null()
    }

    /// 父进程的映像名是不是 `explorer.exe`。
    ///
    /// 双击、开始菜单、桌面快捷方式都经 `explorer.exe`。从终端敲进来的父进程是
    /// `cmd.exe` / `pwsh.exe` / `WindowsTerminal.exe`，不会是它。
    fn parent_is_explorer() -> bool {
        parent_image_name()
            .map(|name| name.eq_ignore_ascii_case("explorer.exe"))
            .unwrap_or(false)
    }

    /// 父进程的映像名。
    ///
    /// 拿不到时返回 `None`（`?` 在调用方折成「不是图形会话」）：判不出来就当终端，
    /// 与三层判定的方向一致。
    fn parent_image_name() -> Option<String> {
        use windows_sys::Win32::Foundation::{CloseHandle, INVALID_HANDLE_VALUE};
        use windows_sys::Win32::System::Diagnostics::ToolHelp::{
            CreateToolhelp32Snapshot, PROCESSENTRY32W, Process32FirstW, Process32NextW,
            TH32CS_SNAPPROCESS,
        };
        use windows_sys::Win32::System::Threading::GetCurrentProcessId;

        // SAFETY: 快照句柄由我们创建、由我们关闭；`entry` 是零初始化的有效缓冲区，
        // `dwSize` 必须先填成结构体大小，否则 API 返回失败。
        unsafe {
            let snapshot = CreateToolhelp32Snapshot(TH32CS_SNAPPROCESS, 0);
            if snapshot == INVALID_HANDLE_VALUE {
                return None;
            }
            let me = GetCurrentProcessId();
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut parent = None;
            let mut ok = Process32FirstW(snapshot, &mut entry);
            while ok != 0 {
                if entry.th32ProcessID == me {
                    parent = Some(entry.th32ParentProcessID);
                    break;
                }
                ok = Process32NextW(snapshot, &mut entry);
            }
            let Some(parent_pid) = parent else {
                CloseHandle(snapshot);
                return None;
            };
            // 第二趟找父进程自己的条目。
            let mut entry: PROCESSENTRY32W = std::mem::zeroed();
            entry.dwSize = std::mem::size_of::<PROCESSENTRY32W>() as u32;
            let mut name = None;
            let mut ok = Process32FirstW(snapshot, &mut entry);
            while ok != 0 {
                if entry.th32ProcessID == parent_pid {
                    let end = entry
                        .szExeFile
                        .iter()
                        .position(|&c| c == 0)
                        .unwrap_or(entry.szExeFile.len());
                    name = Some(String::from_utf16_lossy(&entry.szExeFile[..end]));
                    break;
                }
                ok = Process32NextW(snapshot, &mut entry);
            }
            CloseHandle(snapshot);
            name
        }
    }
}

#[cfg(not(any(target_os = "macos", target_os = "linux", target_os = "windows")))]
mod platform {
    /// 三端之外的兜底：没有信号，一律 TUI。
    pub(super) fn graphical_session() -> Option<&'static str> {
        None
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn the_marker_is_removed_and_reported() {
        let (rest, marked) = strip_launch_marker(
            ["qc", "--launched-from-gui", "run", "-p", "hello"]
                .into_iter()
                .map(String::from),
        );
        assert!(marked);
        assert_eq!(rest, ["qc", "run", "-p", "hello"]);
    }

    #[test]
    fn without_the_marker_nothing_is_stripped() {
        let (rest, marked) =
            strip_launch_marker(["qc", "run", "-p", "hello"].into_iter().map(String::from));
        assert!(!marked);
        assert_eq!(rest, ["qc", "run", "-p", "hello"]);
    }

    #[test]
    fn the_marker_wins_over_everything() {
        // 标记是事实，平台信号是推断：带了标记就不过第 2 层。
        let decision = detect_with(true, false);
        assert_eq!(decision.frontend, Frontend::Gui);
        assert_eq!(decision.reason, "launch-marker");
    }

    /// `Info.plist` 的 `LSEnvironment` 那条（macOS 的 `.app` 用），与命令行标记等价。
    #[test]
    fn the_plist_marker_selects_the_window() {
        let decision = detect_with(false, true);
        assert_eq!(decision.frontend, Frontend::Gui);
        assert_eq!(decision.reason, "launch-marker");
    }

    /// 测试进程跑在终端或 CI 里，两种情形都不该被判成 GUI 会话。
    ///
    /// 这条在 mac / linux 上都成立；Windows 上 `cargo test` 挂着控制台，同样成立。
    /// 它同时也是「判定没有副作用、不会拖慢启动」的最低保证——真连一次 X server
    /// 或枚举进程表之外的任何东西，这里会明显变慢。
    ///
    /// 走 `detect_with` 而不是 `detect`：后者读环境，而环境是进程级的，会被并发的
    /// 用例干扰。
    #[test]
    fn the_test_harness_itself_is_not_a_gui_session() {
        let decision = detect_with(false, false);
        assert_eq!(
            decision.frontend,
            Frontend::Tui,
            "测试进程被判成 GUI 会话了：{}",
            decision.reason
        );
    }
}
