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

/// 激活设置窗口：Windows 上不做 `activate_window` 的合成输入注入，避免跨窗口
/// update 时触发同窗口的输入派发导致 App 双重借用；其它平台行为同 [`show_window`]。
pub fn show_settings_window(window: &gpui_kit::Window) {
    #[cfg(target_os = "windows")]
    windows::show_window_no_activate(window);
    #[cfg(not(target_os = "windows"))]
    show_window(window);
}

/// Windows：用原生 HWND 直接还原/置顶设置窗口，完全不经过 GPUI 窗口 `update`，
/// 从根上避开跨窗口 update 期间同步 WM_* 输入派发的双重借用。
#[cfg(target_os = "windows")]
pub fn show_settings_hwnd(hwnd: isize) {
    windows::show_settings_hwnd(hwnd);
}

/// 隐藏主窗口（关闭按钮 = 最小化到托盘）：窗口从任务栏/Dock 消失，只留托盘
/// 图标。
///
/// **只能在没有 App 可变借用时调用**。要在 `on_window_should_close`、动作回调
/// 这类已经持有 `&mut App` 的地方隐藏窗口，用 [`hide_window_later`]。
pub fn hide_window(window: &gpui_kit::Window) {
    #[cfg(target_os = "macos")]
    macos::hide_window(window);
    #[cfg(target_os = "windows")]
    windows::hide_window(window);
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    window.minimize_window();
}

/// 隐藏窗口，但把原生调用推迟到本次 App 借用结束之后。
///
/// macOS 的 `orderOut:` 会让另一个窗口立刻成为 key window，AppKit 随即**同步**
/// 派发 `windowDidBecomeKey:`；gpui-pre-macos 在该回调里同步请求一帧
/// （`gpui-pre-macos/src/window.rs:3263`，注释说明这是为了避免 native tab
/// 切换闪烁）。帧请求需要 `App::update`，而 `on_window_should_close` 用
/// `cx.update` 包住回调（`gpui-pre/src/window.rs:7144`），此刻正持有
/// `&mut App`，于是每次关窗都刷三条错误：
///
/// ```text
/// ERROR reading power state for frame request for window WindowId(2v1):
///   RefCell already borrowed: attempted mutable App borrow at .../window.rs:1930;
///   existing mutable borrow group began at .../window.rs:7144
/// ```
///
/// Windows 侧同源：`SW_HIDE` 会让窗口同步收到 `WM_ACTIVATE`，被 WndProc 派发
/// 成输入事件后再借用 App。`show_settings_hwnd` 的注释记录过同类修复。
///
/// 因此这里只做“读句柄”（纯读取，无副作用），原生调用交给 foreground
/// executor 在 update 结束后执行——那时没有任何 App 借用。
pub fn hide_window_later(window: &gpui_kit::Window, cx: &gpui_kit::App) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        #[cfg(target_os = "macos")]
        let target = macos::capture(window);
        #[cfg(target_os = "windows")]
        let target = windows::capture(window);
        cx.foreground_executor()
            .spawn(async move {
                if let Some(target) = target {
                    #[cfg(target_os = "macos")]
                    macos::hide_native(target);
                    #[cfg(target_os = "windows")]
                    windows::hide_native(target);
                }
            })
            .detach();
    }
    // 其余平台没有可捕获的裸句柄，退回 GPUI 的最小化，仍然异步执行。
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    {
        let handle = window.window_handle();
        let mut async_cx = cx.to_async();
        cx.foreground_executor()
            .spawn(async move {
                let _ = handle.update(&mut async_cx, |_, window, _| window.minimize_window());
            })
            .detach();
    }
}

/// 把窗口从托盘恢复到前台，原生调用推迟到本次 App 借用结束之后。
///
/// 与 [`hide_window_later`] 同理：`makeKeyAndOrderFront:` / `SetForegroundWindow`
/// 会同步让窗口成为 key window，AppKit/WndProc 随即派发激活事件并在回调里请求
/// 一帧，而调用方（托盘命令处理）此刻正持有 `&mut App`。
pub fn show_window_later(window: &gpui_kit::Window, cx: &gpui_kit::App) {
    #[cfg(any(target_os = "macos", target_os = "windows"))]
    {
        #[cfg(target_os = "macos")]
        let target = macos::capture(window);
        #[cfg(target_os = "windows")]
        let target = windows::capture(window);
        // `Window::activate_window` 在 gpui 内部已经 spawn 到 foreground
        // executor（gpui-pre-macos `PlatformWindow::activate`），同步部分只剩
        // 下面这些原生调用。
        window.activate_window();
        cx.foreground_executor()
            .spawn(async move {
                if let Some(target) = target {
                    #[cfg(target_os = "macos")]
                    macos::show_native(target);
                    #[cfg(target_os = "windows")]
                    windows::show_native(target);
                }
            })
            .detach();
    }
    #[cfg(not(any(target_os = "macos", target_os = "windows")))]
    window.activate_window();
}
