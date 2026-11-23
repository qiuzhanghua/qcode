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
pub(super) const CHANNEL_EXT: &str = "channel";

pub(super) struct LockGuard;

pub(super) struct Listener;

pub(super) fn channel_address(paths: &super::Paths) -> String {
    paths.channel.display().to_string()
}

/// 这个平台没有限制，通道永远落在 data dir 里（与 Windows 同）。
pub(super) fn channel_path(
    scope: super::Scope,
    data_dir: &Path,
) -> (std::path::PathBuf, Option<String>) {
    (super::channel_in_data_dir(scope, data_dir), None)
}

pub(super) fn lock(_path: &Path) -> io::Result<Option<LockGuard>> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        qcode_i18n::tr("core.err.single_instance.no_lock").to_string(),
    ))
}

pub(super) fn try_request(_paths: &super::Paths, _request: super::Request) -> io::Result<bool> {
    Ok(false)
}

pub(super) fn listen(_paths: &super::Paths) -> io::Result<Listener> {
    Err(io::Error::new(
        io::ErrorKind::Unsupported,
        qcode_i18n::tr("core.err.single_instance.no_activation").to_string(),
    ))
}

impl Listener {
    pub(super) fn accept_loop<F: Fn(super::Request, &mut dyn std::io::Write) -> bool>(
        self,
        _on_request: F,
    ) {
    }
}
