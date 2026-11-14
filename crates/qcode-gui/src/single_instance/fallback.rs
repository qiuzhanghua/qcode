//! 三端之外的兜底：没有实现，永远退回「不互斥」。
//!
//! 存在的理由只是让 `mod.rs` 不必为「第三种平台」散落一堆 `cfg`。
//! 这些类型**永远不会被构造**——`lock` 一定返回错误，于是 [`acquire`] 会走
//! `Disabled` 分支。
//!
//! [`acquire`]: super::acquire

use std::io;
use std::path::Path;

/// 这个平台上通道没有落脚点。
pub(super) const CHANNEL_NAME: &str = "gui.channel";

pub(super) struct LockGuard;

pub(super) struct Listener;

pub(super) fn channel_address(paths: &super::Paths) -> String {
    paths.channel.display().to_string()
}

pub(super) fn lock(_path: &Path) -> io::Result<Option<LockGuard>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "single-instance locking is not implemented on this platform",
    ))
}

pub(super) fn try_activate(_paths: &super::Paths) -> io::Result<bool> {
    Ok(false)
}

pub(super) fn listen(_paths: &super::Paths) -> io::Result<Listener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        "single-instance activation is not implemented on this platform",
    ))
}

impl Listener {
    pub(super) fn accept_loop(self, _tx: std::sync::mpsc::Sender<super::TrayCommand>) {}
}
