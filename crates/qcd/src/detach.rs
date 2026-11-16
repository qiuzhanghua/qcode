//! 分离启动 `qcd` 的**实现**已经搬到 [`qcode_core::daemon`]（ADR-0098 §1）。
//!
//! 留这个模块只为一件事：`qcd` 自己与它的测试仍然从 `qcd::detach` 取，
//! 而实现只有一份。为什么搬：GUI 与 TUI 也要「起一个 daemon」，而它们不该
//! 依赖 `qcd` 这个包——那是 ADR-0092 §2 要避免的依赖面纠缠（前端 → daemon）。
//!
//! 这段代码本身没有变过：`setsid` / `DETACHED_PROCESS` 的两平台差别、不双 fork、
//! 不写 pid 文件的理由，都跟着实现一起搬到了新位置。

pub use qcode_core::daemon::{NotInstalled, find_qcd, spawn};
