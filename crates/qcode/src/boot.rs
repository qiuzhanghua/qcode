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
///
/// 定义在 `qcode_core::launch`：`run_shell` 也要按名字把这个标记从子进程环境里
/// 剔掉（见 [`consume_env_marker`] 的说明）。
pub const LAUNCHED_FROM_GUI: &str = qcode_core::launch::MARKER_ARG;

/// `Info.plist` 的 `LSEnvironment` 挂进来的标记（ADR-0086 §4）。
///
/// 内部参数 [`LAUNCHED_FROM_GUI`] 是给能改命令行的入口用的（`.desktop` 的
/// `Exec=`、`.lnk` 的 target、开发者手敲）。macOS 的 `.app` 没有命令行可改——
/// 双击时系统按 `Info.plist` 直接 exec `CFBundleExecutable`——所以那一条路上
/// 挂的是这个环境变量。两条路都是「我们自己的入口显式说一声」，强度一样。
pub const GUI_MARKER_ENV: &str = qcode_core::launch::GUI_MARKER_ENV;

/// 读走环境里的启动标记，并**立刻从本进程环境里删掉**（ADR-0089 §5）。
///
/// 为什么必须删：环境变量会被子进程继承，而 qc 会启动很多子进程——`run_shell`
/// 的 shell、GUI 的内建终端、环境采样的 login shell。它们都是**用户的**终端，
/// 在那种 shell 里敲 `qc` 本该打印用法，却会因为继承了这个标记而再开一个窗口
/// （实测：标记还会经由采样进入 `qc shell-env` 看到的那份快照）。
///
/// 为什么在这里删而不是逐个在子进程处剔除：GUI 的内建终端走的是 alacritty 的
/// `Options.env`，它只能**追加**、不能删；从本进程环境里去掉是唯一能覆盖它的
/// 做法。子进程边界处的显式剔除仍然保留（`run_shell`），那是防用户 rc 文件
/// 或 `env_include` 把它又带回来。
///
/// **只能在单线程阶段调用**：`std::env::remove_var` 在 edition 2024 是 `unsafe`，
/// 因为它改的是进程级 `environ`。`main()` 里调用点位于 tokio 运行时创建之前、
/// `procguard::enable()` 之后（那个函数只置一个标志，不起线程）。
pub fn consume_env_marker() -> bool {
    let present = std::env::var_os(GUI_MARKER_ENV).is_some();
    if present {
        // SAFETY: 调用点在 tokio 运行时与任何线程创建之前（见函数文档），
        // 此刻没有第二个线程能并发读 `environ`。
        unsafe { std::env::remove_var(GUI_MARKER_ENV) };
    }
    present
}

/// 这次该开哪种前端。
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum Frontend {
    /// 终端界面。**不是「默认动作」**：裸 `qc` 落到这一层时打印用法，
    /// 要进 TUI 得敲 `qc tui`（ADR-0089）。
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
/// `marked` 来自 [`strip_launch_marker`]，`env_marker` 来自 [`consume_env_marker`]
/// ——**两者都由调用方在单线程阶段读好**：`consume_env_marker` 会顺手把环境变量
/// 删掉（ADR-0089 §5），所以本函数不能再自己去读，否则读到的是已经删过的环境。
pub fn detect(marked: bool, env_marker: bool) -> Decision {
    detect_with(marked, env_marker)
}

/// 判定主体，**不读环境**。
///
/// 环境读取留在这里之外，是为了能测：`std::env` 是进程级的，而 `cargo test` 的用例
/// 是并发线程。第一版直接在这样的函数里读环境变量，于是「设定标记」和「清除标记」
/// 两条用例互相看到对方的值，随机红——这条注释就是那次失败的墓志铭。
fn detect_with(marked: bool, env_marker: bool) -> Decision {
    detect_with_signal(marked, env_marker, platform::graphical_session)
}

/// [`detect_with`], but with the platform signal supplied by the caller.
///
/// This layer exists because the test that used to sit here was fooling itself.
/// `the_test_harness_itself_is_not_a_gui_session` went through `detect_with`, whose
/// own comment claimed it does not read the environment — while its second layer is
/// `platform::graphical_session()`, which reads `std::env` and `isatty(stdin)`. So the
/// test asserted "this machine does not look like a graphical session right now",
/// which is false on a desktop host whose stdin is not a tty (a sandbox, an IDE
/// runner, `docker exec` without `-t`), and it failed while the decision code was
/// perfectly correct. A test cannot check a decision it cannot replace.
fn detect_with_signal(
    marked: bool,
    env_marker: bool,
    signal: impl FnOnce() -> Option<&'static str>,
) -> Decision {
    if marked || env_marker {
        return Decision {
            frontend: Frontend::Gui,
            reason: "launch-marker",
        };
    }
    match signal() {
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

/// 三端判定的**纯逻辑**，与采集分开。
///
/// 采集（`isatty`、`getppid`、`GetConsoleWindow`、`std::env::var`）每次只有几行，而
/// 历史上出错的一直是**判断**：把「没有 tty」当成「不是 GUI」、漏掉 `DISPLAY` 的必要性、
/// 把裸词当成级别名（同一个星期里在 `logging` 那边刚犯过一次）。判断因此搬到能测的地方。
///
/// 三个 `*Facts` 都**不按平台 `cfg`**：CI 只在宿主那一端跑测试，而 macOS 的规则恰恰最
/// 难在手上验证。放在这里，Linux 上也能测 macOS 与 Windows 的判定表。
mod signals {
    // 非宿主平台的规则在**非测试构建**里没有调用者——那正是它们存在的理由：让 Linux
    // 也能测 macOS 的判定表。`cfg_attr(not(test), …)` 而不是无条件 `allow`：在测试构建里
    // 如果没人用（比如测试被删掉），仍然会报。
    #![cfg_attr(not(test), allow(dead_code))]

    use std::ffi::OsStr;

    /// macOS 判定所需的事实。
    pub(super) struct MacFacts<'a> {
        /// `TERM` 存在——**不能**用作「在终端里」的证据：`open` 会把调用者的环境带进来，
        /// 实测从终端 `open -a` 起的进程带着 `TERM=xterm-ghostty`。它只能反过来用：
        /// 有就一定是终端启动。
        pub(super) term: Option<&'a OsStr>,
        /// 同上，`TERM_PROGRAM`。
        pub(super) term_program: Option<&'a OsStr>,
        /// 有终端设备就说明是终端里启动的：比父进程名可靠（父进程可能是任何东西）。
        pub(super) stdin_is_tty: bool,
        pub(super) parent_is_launchd: bool,
        pub(super) bundle_id: Option<&'a str>,
    }

    /// macOS：GUI 会话的两个独立信号，**必须同时成立**。
    ///
    /// 实测（本机，一个只自 dump 环境与父进程的最小 `.app`）：
    ///
    /// | 启动方式 | 父进程 | `__CFBundleIdentifier` |
    /// |---|---|---|
    /// | 终端直接 exec | `/bin/zsh` | 终端自己的（`com.mitchellh.ghostty`） |
    /// | `env -i open -a`（= 双击） | `launchd`（pid 1） | 我们自己的 |
    ///
    /// 单独看任意一个都会误判：`setsid` 起的进程父进程也不是 shell，而从终端
    /// `open -a` 时 `__CFBundleIdentifier` 已经是我们自己了（父进程也是 `launchd`）
    /// ——那种情况**确实**是图形启动，判成 GUI 是对的。
    pub(super) fn macos(facts: &MacFacts<'_>) -> Option<&'static str> {
        if facts.term.is_some() || facts.term_program.is_some() {
            return None;
        }
        if facts.stdin_is_tty {
            return None;
        }
        let bundle_is_ours = facts
            .bundle_id
            .is_some_and(|id| id.starts_with("com.qcode."));
        if facts.parent_is_launchd && bundle_is_ours {
            return Some("coder-macos-launchd");
        }
        None
    }

    /// Linux 判定所需的事实。
    pub(super) struct LinuxFacts<'a> {
        pub(super) stdin_is_tty: bool,
        pub(super) session_type: &'a str,
        pub(super) wayland_display: Option<&'a OsStr>,
        pub(super) display: Option<&'a OsStr>,
    }

    /// Linux 上的两个独立信号：没有终端设备，且处在图形会话里。
    pub(super) fn linux(facts: &LinuxFacts<'_>) -> Option<&'static str> {
        if facts.stdin_is_tty {
            return None;
        }
        // `.desktop` 里我们写 `Terminal=false`，于是那条进程没有控制终端。但「没有 tty」
        // 也可能是 systemd unit / cron / CI，所以还要图形会话的证据。
        if !matches!(facts.session_type, "wayland" | "x11" | "mir") {
            return None;
        }
        // 没有 DISPLAY / WAYLAND_DISPLAY 就打不开窗口，别把人送进一条必然失败的路——
        // 那比留他在 TUI 里更糟。
        if facts.wayland_display.is_none() && facts.display.is_none() {
            return None;
        }
        Some("coder-linux-desktop")
    }

    /// Windows 判定所需的事实。
    pub(super) struct WindowsFacts<'a> {
        pub(super) has_console: bool,
        pub(super) parent_image: Option<&'a str>,
    }

    /// Windows 上的两个独立信号：没有控制台窗口，且父进程是资源管理器。
    ///
    /// `qc` 是控制台子系统程序，从 `cmd.exe` / `pwsh` / Windows Terminal 启动时一定
    /// 挂着控制台；双击时 Windows 会给它新建一个**自己的**控制台，那种情形要靠父进程
    /// 区分。父进程名拿不到时按终端处理：判不出来就当终端，与三层判定的方向一致。
    pub(super) fn windows(facts: &WindowsFacts<'_>) -> Option<&'static str> {
        if facts.has_console {
            return None;
        }
        let parent_is_explorer = facts
            .parent_image
            .is_some_and(|name| name.eq_ignore_ascii_case("explorer.exe"));
        if parent_is_explorer {
            return Some("coder-windows-explorer");
        }
        None
    }
}

#[cfg(target_os = "macos")]
/// macOS：采集事实，判断在 [`signals::macos`]。
mod platform {
    pub(super) fn graphical_session() -> Option<&'static str> {
        super::signals::macos(&super::signals::MacFacts {
            term: std::env::var_os("TERM").as_deref(),
            term_program: std::env::var_os("TERM_PROGRAM").as_deref(),
            stdin_is_tty: stdin_is_tty(),
            parent_is_launchd: parent_is_launchd(),
            bundle_id: std::env::var("__CFBundleIdentifier").ok().as_deref(),
        })
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
}

#[cfg(target_os = "linux")]
/// Linux：采集事实，判断在 [`signals::linux`]。
mod platform {
    pub(super) fn graphical_session() -> Option<&'static str> {
        let session_type = std::env::var("XDG_SESSION_TYPE").unwrap_or_default();
        let wayland_display = std::env::var_os("WAYLAND_DISPLAY");
        let display = std::env::var_os("DISPLAY");
        super::signals::linux(&super::signals::LinuxFacts {
            stdin_is_tty: stdin_is_tty(),
            session_type: &session_type,
            wayland_display: wayland_display.as_deref(),
            display: display.as_deref(),
        })
    }

    fn stdin_is_tty() -> bool {
        // SAFETY: `isatty` 只读一个 fd，对任意整数都安全。
        unsafe { libc::isatty(libc::STDIN_FILENO) == 1 }
    }
}

#[cfg(windows)]
/// Windows：采集事实，判断在 [`signals::windows`]。
mod platform {
    pub(super) fn graphical_session() -> Option<&'static str> {
        let parent_image = parent_image_name();
        super::signals::windows(&super::signals::WindowsFacts {
            has_console: has_console(),
            parent_image: parent_image.as_deref(),
        })
    }

    /// 有没有控制台。`qc` 是控制台子系统程序，从 `cmd.exe` / `pwsh` / Windows
    /// Terminal 启动时一定挂着控制台；双击时 Windows 会给它新建一个**自己的**
    /// 控制台——那种情形要靠父进程区分。
    fn has_console() -> bool {
        use windows_sys::Win32::System::Console::GetConsoleWindow;
        // SAFETY: 无参数、无副作用；返回 0 表示没有控制台。
        !unsafe { GetConsoleWindow() }.is_null()
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
    /// 三端之外的兜底：没有信号，一律当作终端启动（ADR-0089 之后这意味着
    /// 「裸 `qc` 打印用法」，而不是「进 TUI」）。
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

    /// 没有标记、平台也没给信号时必须落到终端。
    ///
    /// 走 [`detect_with_signal`] 并**显式传 `None`**：这条要断言的是「没有正面证据就
    /// 不弹窗」这条规则，而不是「本机此刻看起来不像图形会话」。原来的版本让测试进程
    /// 自己的环境参与判定，于是在没有 tty 的桌面会话里（沙箱、IDE runner、`docker exec`
    /// 不带 `-t`）必然红，而判定代码一行没错。
    #[test]
    fn no_marker_and_no_signal_is_the_terminal() {
        let decision = detect_with_signal(false, false, || None);
        assert_eq!(decision.frontend, Frontend::Tui);
        assert_eq!(decision.reason, "no-gui-signal");
    }

    /// 平台给出信号才走窗口，且信号原样进日志——`reason` 是排查「点了图标没反应」时
    /// 唯一能 grep 的线索。
    #[test]
    fn a_platform_signal_selects_the_window() {
        let decision = detect_with_signal(false, false, || Some("coder-linux-desktop"));
        assert_eq!(decision.frontend, Frontend::Gui);
        assert_eq!(decision.reason, "coder-linux-desktop");
    }

    /// 标记优先于平台信号：带了标记就**不去问**平台。
    ///
    /// 断言用的是「一被调用就 panic」的闭包，所以它同时证明那次探测根本没发生——
    /// 否则又是一条「看起来测了顺序」的测试。
    #[test]
    fn the_marker_short_circuits_the_platform_probe() {
        let decision = detect_with_signal(true, false, || {
            panic!("the platform must not be probed when the marker is present")
        });
        assert_eq!(decision.frontend, Frontend::Gui);
        assert_eq!(decision.reason, "launch-marker");
    }

    /// macOS 的判定表，**在 Linux 上也要跑**（`signals` 不带平台 cfg）。
    ///
    /// 这张表是本机最难验证的东西：手上没有 mac、CI 的 mac job 只跑自己那一端。把判断
    /// 与采集分开之后，它就成了普通函数。
    #[test]
    fn the_macos_rules() {
        /// 双击：launchd 的子进程，我们自己的 bundle，没有终端。
        const DOUBLE_CLICK: signals::MacFacts<'_> = signals::MacFacts {
            term: None,
            term_program: None,
            stdin_is_tty: false,
            parent_is_launchd: true,
            bundle_id: Some("com.qcode.app"),
        };

        assert_eq!(signals::macos(&DOUBLE_CLICK), Some("coder-macos-launchd"));

        // 终端里启动：`TERM` 就是证据（`open` 会把调用者的环境带进来，所以这个变量
        // 只能用来证伪，不能用来证明）。
        let term_some = std::ffi::OsStr::new("xterm-ghostty");
        assert_eq!(
            signals::macos(&signals::MacFacts {
                term: Some(term_some),
                ..DOUBLE_CLICK
            }),
            None
        );
        assert_eq!(
            signals::macos(&signals::MacFacts {
                term_program: Some(std::ffi::OsStr::new("ghostty")),
                ..DOUBLE_CLICK
            }),
            None
        );
        assert_eq!(
            signals::macos(&signals::MacFacts {
                stdin_is_tty: true,
                ..DOUBLE_CLICK
            }),
            None,
            "a tty is decisive: it beats the parent-process and bundle evidence"
        );
        // 两个信号缺一不可：漏掉 bundle 判定会把 `setsid` 起的进程弹成窗口。
        assert_eq!(
            signals::macos(&signals::MacFacts {
                parent_is_launchd: false,
                ..DOUBLE_CLICK
            }),
            None
        );
        assert_eq!(
            signals::macos(&signals::MacFacts {
                bundle_id: None,
                ..DOUBLE_CLICK
            }),
            None
        );
        // 从终端 `open -a` 起的进程带着终端的 bundle id：不是我们的。
        assert_eq!(
            signals::macos(&signals::MacFacts {
                bundle_id: Some("com.mitchellh.ghostty"),
                ..DOUBLE_CLICK
            }),
            None
        );
    }

    /// Linux 的判定表。`DISPLAY` 那一格是真实事故的形状：没有它窗口打不开，那时宁可
    /// 留在 TUI，也不要送用户进一条必然失败的路。
    #[test]
    fn the_linux_rules() {
        let display = std::ffi::OsStr::new(":1");
        let desktop = signals::LinuxFacts {
            stdin_is_tty: false,
            session_type: "x11",
            wayland_display: None,
            display: Some(display),
        };
        assert_eq!(signals::linux(&desktop), Some("coder-linux-desktop"));

        assert_eq!(
            signals::linux(&signals::LinuxFacts {
                wayland_display: Some(std::ffi::OsStr::new("wayland-0")),
                display: None,
                ..desktop
            }),
            Some("coder-linux-desktop"),
            "wayland is a display too"
        );
        assert_eq!(
            signals::linux(&signals::LinuxFacts {
                stdin_is_tty: true,
                ..desktop
            }),
            None
        );
        // 没有 tty 但也没有图形会话：systemd unit / cron / CI，全都该留在终端。
        for session in ["", "tty", "cron"] {
            assert_eq!(
                signals::linux(&signals::LinuxFacts {
                    session_type: session,
                    ..desktop
                }),
                None,
                "session_type {session:?}"
            );
        }
        assert_eq!(
            signals::linux(&signals::LinuxFacts {
                wayland_display: None,
                display: None,
                ..desktop
            }),
            None,
            "a graphical session with nowhere to draw is still a bad guess"
        );
    }

    /// Windows 的判定表。
    #[test]
    fn the_windows_rules() {
        // 双击：没有控制台，父进程是 explorer.exe（大小写不敏感——实测两种都出现过）。
        assert_eq!(
            signals::windows(&signals::WindowsFacts {
                has_console: false,
                parent_image: Some("explorer.exe"),
            }),
            Some("coder-windows-explorer")
        );
        assert_eq!(
            signals::windows(&signals::WindowsFacts {
                has_console: false,
                parent_image: Some("EXPLORER.EXE"),
            }),
            Some("coder-windows-explorer")
        );
        // 从终端启动：一定挂着自己的控制台。
        assert_eq!(
            signals::windows(&signals::WindowsFacts {
                has_console: true,
                parent_image: Some("explorer.exe"),
            }),
            None
        );
        for parent in ["cmd.exe", "pwsh.exe", "WindowsTerminal.exe", ""] {
            assert_eq!(
                signals::windows(&signals::WindowsFacts {
                    has_console: false,
                    parent_image: Some(parent),
                }),
                None,
                "parent {parent:?}"
            );
        }
        // 拿不到父进程名：判不出来就当终端（三层判定的方向）。
        assert_eq!(
            signals::windows(&signals::WindowsFacts {
                has_console: false,
                parent_image: None,
            }),
            None
        );
    }

    /// `.desktop` 的 `Exec=` 必须通过**内部标记**要窗口，而不是某个子命令。
    ///
    /// 这条断言原来在 `qcode-gui` 的 `window_icon::probes` 里，写死「`Exec` 第二个词
    /// 等于 `gui`」；那个模块是 `#[cfg(all(test, target_os = "linux"))]`，所以 ADR-0086
    /// 把 `Exec=` 改掉之后本机全绿、CI 才红。搬到这里的理由有两个：常量在这个 crate
    /// 里，断言可以直接引用它（不重复字面量，也就不会各自漂移）；而且这里三端都跑。
    #[test]
    fn the_desktop_entry_launches_the_window_with_the_marker() {
        let entry = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/res/linux/qcode.desktop"
        ));
        let exec = entry
            .lines()
            .find_map(|line| line.strip_prefix("Exec="))
            .expect("the desktop entry must have an Exec=")
            .trim();
        let mut words = exec.split_whitespace();
        assert_eq!(
            words.next(),
            Some("qc"),
            "the installer rewrites the path, not the program"
        );
        assert_eq!(
            words.next(),
            Some(LAUNCHED_FROM_GUI),
            "the entry has to open the window through the launch marker, not a subcommand"
        );
    }

    /// `install-linux-desktop.sh` 重写 `Exec=` 时必须写同一个标记。
    ///
    /// 它和 `.desktop` 是两份东西：装到 `~/.local/share/applications` 的那份由脚本
    /// 用 `sed` 重写。两边漂了，装出来的条目就退回 TUI——而「点了没反应」是最难查的
    /// 那种报告。
    ///
    /// 脚本内部把这个标记收成了一个变量（`--check` 要验证它、`sed` 要写入它，两处
    /// 各写一份字面量同样是漂移源），所以这里**两条都断言**：定义里是这个标记，
    /// 且重写用的就是这个变量。只查其中一条时，另一条被改成别的值不会被发现。
    #[test]
    fn the_linux_installer_writes_the_same_marker() {
        let script = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../../scripts/install-linux-desktop.sh"
        ));
        assert!(
            script.contains(&format!("MARKER_ARG={LAUNCHED_FROM_GUI}")),
            "install-linux-desktop.sh must define MARKER_ARG={LAUNCHED_FROM_GUI}"
        );
        assert!(
            script.contains("Exec=$exec_cmd $MARKER_ARG"),
            "install-linux-desktop.sh must rewrite Exec= with $MARKER_ARG"
        );
    }
}
