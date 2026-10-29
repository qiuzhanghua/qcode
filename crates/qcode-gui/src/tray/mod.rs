//! 系统托盘（ADR-0052）：Linux 用 ksni，macOS 用 NSStatusItem，Windows 用
//! Shell_NotifyIcon。三个后端共用同一份 `TraySnapshot` / `TrayCommand`。

pub mod state;

pub use state::{TrayCommand, TraySnapshot};

#[cfg(target_os = "linux")]
mod linux;
#[cfg(target_os = "macos")]
mod macos;
#[cfg(target_os = "windows")]
mod windows;

#[cfg(target_os = "linux")]
pub struct TrayHandle {
    inner: linux::LinuxTray,
}

#[cfg(target_os = "macos")]
pub struct TrayHandle {
    inner: macos::MacTray,
}

#[cfg(target_os = "windows")]
pub struct TrayHandle {
    inner: windows::WindowsTray,
}

#[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
pub struct TrayHandle;

impl TrayHandle {
    pub fn update(&self, snapshot: TraySnapshot) {
        #[cfg(any(target_os = "linux", target_os = "macos", target_os = "windows"))]
        self.inner.update(snapshot);
        #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
        let _ = snapshot;
    }
}

/// 启动托盘；平台不支持或注册失败时返回 `None`，调用方退回“关窗即退”。
pub fn start() -> (Option<TrayHandle>, std::sync::mpsc::Receiver<TrayCommand>) {
    let (tx, rx) = std::sync::mpsc::channel();

    #[cfg(target_os = "linux")]
    let handle = linux::start(tx).map(|inner| TrayHandle { inner });
    #[cfg(target_os = "macos")]
    let handle = macos::start(tx).map(|inner| TrayHandle { inner });
    #[cfg(target_os = "windows")]
    let handle = windows::start(tx).map(|inner| TrayHandle { inner });
    #[cfg(not(any(target_os = "linux", target_os = "macos", target_os = "windows")))]
    let handle = {
        let _ = tx;
        None
    };

    (handle, rx)
}

/// 把主窗口从最小化恢复到前台。
///
/// GPUI 的 `activate_window()` 只负责激活，不会还原最小化的窗口，所以各平台
/// 先做原生的 restore（macOS `deminiaturize:` / Windows `SW_RESTORE`）再激活。
pub fn show_window(window: &gpui_kit::Window) {
    #[cfg(target_os = "macos")]
    macos::show_window(window);
    #[cfg(target_os = "windows")]
    windows::show_window(window);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    window.activate_window();
}

/// 隐藏主窗口（关闭按钮 = 最小化到托盘）：窗口从任务栏/Dock 消失，只留托盘
/// 图标。Windows 用 `SW_HIDE`，macOS 用 `orderOut:`，其余平台退回最小化。
pub fn hide_window(window: &gpui_kit::Window) {
    #[cfg(target_os = "macos")]
    macos::hide_window(window);
    #[cfg(target_os = "windows")]
    windows::hide_window(window);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    window.minimize_window();
}
