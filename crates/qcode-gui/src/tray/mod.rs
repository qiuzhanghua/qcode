//! 系统托盘（ADR-0052）：Linux 用 ksni 的 StatusNotifierItem，其他平台暂时为空。

pub mod state;

pub use state::{TrayCommand, TraySnapshot};

#[cfg(target_os = "linux")]
mod linux;

#[cfg(target_os = "linux")]
pub struct TrayHandle {
    inner: linux::LinuxTray,
}

#[cfg(not(target_os = "linux"))]
pub struct TrayHandle;

impl TrayHandle {
    pub fn update(&self, snapshot: TraySnapshot) {
        #[cfg(target_os = "linux")]
        self.inner.update(snapshot);
        #[cfg(not(target_os = "linux"))]
        let _ = snapshot;
    }

    #[allow(dead_code)] // 进程退出时 DBus 名字会自然消失；保留给将来的优雅关闭。
    pub fn shutdown(&self) {
        #[cfg(target_os = "linux")]
        self.inner.shutdown();
    }
}

/// 启动托盘；没有 StatusNotifierWatcher 时返回 `None`，调用方退回“关窗即退”。
pub fn start() -> (Option<TrayHandle>, std::sync::mpsc::Receiver<TrayCommand>) {
    let (tx, rx) = std::sync::mpsc::channel();
    #[cfg(target_os = "linux")]
    let handle = linux::start(tx).map(|inner| TrayHandle { inner });
    #[cfg(not(target_os = "linux"))]
    let handle = {
        let _ = tx;
        None
    };
    (handle, rx)
}
