//! 「这次是怎么被拉起来的」——启动形态标记的**唯一定义处**。
//!
//! 标记有两个载体（ADR-0086 §1），强度一样，都是「我们自己的入口显式说一声」：
//!
//! - argv 里的 [`MARKER_ARG`]，用于 `.desktop` 的 `Exec=`、`.lnk` 的目标；
//! - 环境变量 [`GUI_MARKER_ENV`]，用于 macOS 的 `Info.plist` `LSEnvironment`——
//!   那里的可执行文件就是我们自己，没有地方加参数。
//!
//! **为什么定义放在 `qcode-core` 而不是 `qc` 二进制里**：这不只是判定层要知道的事。
//! 环境变量会被**子进程继承**，而 qc 恰好会启动大量子进程（`run_shell` 的 shell、
//! GUI 的内建终端、环境采样的 login shell）。它们都必须能按名字把这个标记剔掉，
//! 否则「GUI 里启动的 shell 中敲 `qc`」会再开一个窗口而不是打印用法。字符串在
//! 两个 crate 里各写一份，就是那种改名时只会静默失效的重复。

/// 环境变量形式的启动标记。
///
/// 值为 `1`；判定只关心**存在**（见 `qcode::boot::detect`）。空值也算存在——
/// 这是 `var_os(..).is_some()` 的语义，不要改成比较值。
pub const GUI_MARKER_ENV: &str = "QCODE_LAUNCHED_FROM_GUI";

/// argv 形式的启动标记（内部参数，不进 `--help`）。
pub const MARKER_ARG: &str = "--launched-from-gui";
