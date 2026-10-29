//! Windows 托盘：Shell_NotifyIcon + 弹出菜单（ADR-0052 的 Windows 后端）。
//!
//! 托盘消息窗口跑在自己的线程上（GPUI 主线程不跑 Win32 消息循环），菜单点击
//! 通过 `Sender<TrayCommand>` 回传；快照从 GPUI 主线程写进共享状态，再用
//! `TRAY_REFRESH` 让托盘线程刷新 tooltip。

use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};
use std::time::Duration;

use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
use windows_sys::Win32::UI::Shell::{
    NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NIM_MODIFY, NOTIFYICONDATAW,
    Shell_NotifyIconW,
};
use windows_sys::Win32::UI::WindowsAndMessaging::{
    AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu, DispatchMessageW,
    GetCursorPos, GetMessageW, GetSystemMetrics, HMENU, HWND_MESSAGE, IMAGE_ICON, IsIconic,
    IsWindowVisible, LoadImageW, MF_GRAYED, MF_SEPARATOR, MF_STRING, MSG, PostMessageW,
    PostQuitMessage, RegisterClassExW, SM_CXSMICON, SM_CYSMICON, SW_HIDE, SW_RESTORE, SW_SHOW,
    SetForegroundWindow, ShowWindow, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu,
    TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_NULL,
    WM_RBUTTONUP, WNDCLASSEXW,
};

use super::state::{TrayCommand, TraySnapshot};

const TRAY_MSG: u32 = WM_APP + 0x100;
const TRAY_REFRESH: u32 = WM_APP + 0x101;
const MENU_SHOW: usize = 1001;
const MENU_NEW_TASK: usize = 1002;
const MENU_STOP: usize = 1003;
const MENU_QUIT: usize = 1004;
const TRAY_ID: usize = 1;

/// NOTIFYICONDATAW 里是裸 HWND/HICON 指针；包一层让 static 成立。
struct SendNid(NOTIFYICONDATAW);
// 只在托盘线程里读写，指针在进程内始终有效。
unsafe impl Send for SendNid {}
unsafe impl Sync for SendNid {}

static COMMANDS: OnceLock<Mutex<Option<Sender<TrayCommand>>>> = OnceLock::new();
static SNAPSHOT: OnceLock<Mutex<TraySnapshot>> = OnceLock::new();
static TRAY_HWND: OnceLock<isize> = OnceLock::new();
static TRAY_NID: OnceLock<Mutex<Option<SendNid>>> = OnceLock::new();

fn utf16(s: &str) -> Vec<u16> {
    s.encode_utf16().chain(std::iter::once(0)).collect()
}

fn send(command: TrayCommand) {
    if let Some(lock) = COMMANDS.get()
        && let Ok(guard) = lock.lock()
        && let Some(tx) = guard.as_ref()
    {
        let _ = tx.send(command);
    }
}

fn snapshot() -> TraySnapshot {
    SNAPSHOT
        .get()
        .and_then(|lock| lock.lock().ok().map(|guard| guard.clone()))
        .unwrap_or_default()
}

pub struct WindowsTray;

impl WindowsTray {
    pub fn update(&self, snapshot: TraySnapshot) {
        if let Some(lock) = SNAPSHOT.get()
            && let Ok(mut guard) = lock.lock()
        {
            *guard = snapshot;
        }
        if let Some(hwnd) = TRAY_HWND.get() {
            unsafe {
                PostMessageW(*hwnd as HWND, TRAY_REFRESH, 0, 0);
            }
        }
    }
}

pub fn start(commands: Sender<TrayCommand>) -> Option<WindowsTray> {
    let _ = COMMANDS.set(Mutex::new(Some(commands)));
    let _ = SNAPSHOT.set(Mutex::new(TraySnapshot::default()));
    let _ = TRAY_NID.set(Mutex::new(None));

    let (ready_tx, ready_rx) = std::sync::mpsc::channel();
    std::thread::Builder::new()
        .name("qcode-tray".into())
        .spawn(move || tray_thread(ready_tx))
        .ok()?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Some(WindowsTray),
        Ok(Err(error)) => {
            eprintln!("qcode-gui: system tray unavailable: {error}");
            None
        }
        Err(_) => {
            eprintln!("qcode-gui: system tray did not start in time");
            None
        }
    }
}

fn tray_thread(ready: Sender<Result<(), String>>) {
    unsafe {
        let class = utf16("QcodeTrayMessageWindow");
        let instance = GetModuleHandleW(std::ptr::null());
        let wc = WNDCLASSEXW {
            cbSize: std::mem::size_of::<WNDCLASSEXW>() as u32,
            lpfnWndProc: Some(tray_wnd_proc),
            hInstance: instance,
            lpszClassName: class.as_ptr(),
            ..Default::default()
        };
        RegisterClassExW(&wc);
        let hwnd = CreateWindowExW(
            0,
            class.as_ptr(),
            class.as_ptr(),
            0,
            0,
            0,
            0,
            0,
            HWND_MESSAGE,
            std::ptr::null_mut(),
            instance,
            std::ptr::null(),
        );
        if hwnd.is_null() {
            let _ = ready.send(Err("cannot create the tray message window".into()));
            return;
        }
        let _ = TRAY_HWND.set(hwnd as isize);

        // 资源 102 是托盘图标（品牌蓝 QC）；101 是彩色应用图标。
        // 用系统的小图标尺寸取图，125%/150% 缩放时不会被拉伸。
        let hicon = LoadImageW(
            instance,
            102 as *const u16,
            IMAGE_ICON,
            GetSystemMetrics(SM_CXSMICON),
            GetSystemMetrics(SM_CYSMICON),
            0,
        );

        let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
        nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
        nid.hWnd = hwnd;
        nid.uID = TRAY_ID as u32;
        nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
        nid.uCallbackMessage = TRAY_MSG;
        nid.hIcon = hicon;
        write_tip(&mut nid, &snapshot().tooltip());

        if Shell_NotifyIconW(NIM_ADD, &nid) == 0 {
            let _ = ready.send(Err("Shell_NotifyIcon(NIM_ADD) failed".into()));
            return;
        }
        if let Some(lock) = TRAY_NID.get()
            && let Ok(mut guard) = lock.lock()
        {
            *guard = Some(SendNid(nid));
        }
        let _ = ready.send(Ok(()));

        let mut msg: MSG = std::mem::zeroed();
        while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
            TranslateMessage(&msg);
            DispatchMessageW(&msg);
        }
    }
}

fn write_tip(nid: &mut NOTIFYICONDATAW, tip: &str) {
    let tip = utf16(tip);
    let len = tip.len().min(nid.szTip.len() - 1);
    nid.szTip[..len].copy_from_slice(&tip[..len]);
}

fn refresh_tip() {
    let Some(lock) = TRAY_NID.get() else {
        return;
    };
    let Ok(mut guard) = lock.lock() else {
        return;
    };
    let Some(nid) = guard.as_mut() else {
        return;
    };
    write_tip(&mut nid.0, &snapshot().tooltip());
    nid.0.uFlags = NIF_TIP;
    unsafe {
        Shell_NotifyIconW(NIM_MODIFY, &nid.0);
    }
}

unsafe extern "system" fn tray_wnd_proc(
    hwnd: HWND,
    msg: u32,
    wparam: WPARAM,
    lparam: LPARAM,
) -> LRESULT {
    if msg == TRAY_MSG {
        match lparam as u32 {
            // 左键 = 显示主窗口（与菜单第一项一致）。
            WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                send(TrayCommand::ShowWindow);
                return 0;
            }
            WM_RBUTTONUP => {
                unsafe {
                    let mut pt = POINT::default();
                    GetCursorPos(&mut pt);
                    show_menu(hwnd, pt.x, pt.y);
                }
                return 0;
            }
            _ => {}
        }
    } else if msg == TRAY_REFRESH {
        refresh_tip();
        return 0;
    } else if msg == WM_COMMAND {
        match (wparam as usize) & 0xffff {
            MENU_SHOW => send(TrayCommand::ShowWindow),
            MENU_NEW_TASK => send(TrayCommand::NewTask),
            MENU_STOP => send(TrayCommand::StopCurrent),
            MENU_QUIT => send(TrayCommand::Quit),
            _ => {}
        }
        return 0;
    } else if msg == WM_DESTROY {
        unsafe {
            if let Some(lock) = TRAY_NID.get()
                && let Ok(mut guard) = lock.lock()
                && let Some(nid) = guard.take()
            {
                Shell_NotifyIconW(NIM_DELETE, &nid.0);
            }
            PostQuitMessage(0);
        }
        return 0;
    }
    unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
}

unsafe fn show_menu(hwnd: HWND, x: i32, y: i32) {
    unsafe {
        let snapshot = snapshot();
        let menu: HMENU = CreatePopupMenu();

        let status = utf16(&snapshot.status_line());
        AppendMenuW(menu, MF_STRING | MF_GRAYED, 0, status.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());

        let show = utf16("显示主窗口");
        AppendMenuW(menu, MF_STRING, MENU_SHOW, show.as_ptr());
        let new_task = utf16("新建任务");
        AppendMenuW(menu, MF_STRING, MENU_NEW_TASK, new_task.as_ptr());
        let stop = utf16("停止当前任务");
        let stop_flags = if snapshot.can_stop() {
            MF_STRING
        } else {
            MF_STRING | MF_GRAYED
        };
        AppendMenuW(menu, stop_flags, MENU_STOP, stop.as_ptr());
        AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
        let quit = utf16("退出");
        AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());

        // 微软文档：给通知图标弹上下文菜单时，调用 TrackPopupMenu 前当前
        // 窗口必须是前台窗口，否则点击菜单外部时菜单不会消失；菜单结束后
        // 还要回一条 WM_NULL，让系统彻底结束菜单的鼠标捕获循环。
        // 托盘消息窗口是 message-only window，SetForegroundWindow 对它可能
        // 无效，但调用无害且对可见 owner 有效，两个 workaround 一起上。
        SetForegroundWindow(hwnd);
        TrackPopupMenu(
            menu,
            TPM_LEFTALIGN | TPM_RIGHTBUTTON,
            x,
            y,
            0,
            hwnd,
            std::ptr::null_mut(),
        );
        PostMessageW(hwnd, WM_NULL, 0, 0);
        DestroyMenu(menu);
    }
}

/// 隐藏主窗口：关闭按钮 = 最小化到托盘（ADR-0052）。用 `SW_HIDE` 而不是
/// `SW_MINIMIZE`——最小化还会把图标留在任务栏，隐藏则整个从任务栏消失，
/// 恢复只能通过托盘图标。
pub fn hide_window(window: &gpui_kit::Window) {
    let Ok(handle) =
        <gpui_kit::Window as raw_window_handle::HasWindowHandle>::window_handle(window)
    else {
        return;
    };
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    unsafe {
        let hwnd = win32.hwnd.get() as isize as *mut core::ffi::c_void;
        ShowWindow(hwnd, SW_HIDE);
    }
}

/// 从最小化/隐藏恢复到前台。`activate_window()` 只激活不还原，所以先
/// SW_RESTORE（最小化）或 SW_SHOW（SW_HIDE 隐藏的窗口）。
pub fn show_window(window: &gpui_kit::Window) {
    let Ok(handle) =
        <gpui_kit::Window as raw_window_handle::HasWindowHandle>::window_handle(window)
    else {
        return;
    };
    let raw_window_handle::RawWindowHandle::Win32(win32) = handle.as_raw() else {
        return;
    };
    unsafe {
        let hwnd = win32.hwnd.get() as isize as *mut core::ffi::c_void;
        if IsIconic(hwnd) != 0 {
            ShowWindow(hwnd, SW_RESTORE);
        } else if IsWindowVisible(hwnd) == 0 {
            ShowWindow(hwnd, SW_SHOW);
        }
        SetForegroundWindow(hwnd);
    }
    window.activate_window();
}
