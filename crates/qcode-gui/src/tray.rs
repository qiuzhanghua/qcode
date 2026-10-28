//! Close-to-tray: a system tray icon (Windows notification area, macOS menu
//! bar, Linux StatusNotifier) plus native window hiding. Closing the main
//! window hides it and keeps the process resident behind the tray icon, the
//! way chat apps like QQ do; the tray offers "show" and "quit".

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// Whether a tray icon is present (tray install succeeded). Read from the
/// window's close callback, which cannot reach into the Shell state.
static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the tray icon is installed and close-to-tray should apply.
pub fn is_active() -> bool {
    TRAY_ACTIVE.load(Ordering::Relaxed)
}

/// What the tray icon asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left-click / "show" menu item: bring the main window back.
    Show,
    /// "Quit" menu item: actually exit the process.
    Quit,
}

/// The tray icon, created once at startup and kept for the process lifetime.
pub struct TrayIcon {
    events: mpsc::Receiver<TrayEvent>,
    // Platform tray state; kept alive as long as the app lives.
    _platform: Box<dyn std::any::Any>,
}

impl TrayIcon {
    /// Create the tray icon. Failure is non-fatal: the app still runs, just
    /// without a tray presence.
    pub fn install() -> Result<Self, String> {
        let (tx, rx) = mpsc::channel();
        let platform = tray::install(tx)?;
        TRAY_ACTIVE.store(true, Ordering::Relaxed);
        Ok(Self {
            events: rx,
            _platform: Box::new(platform),
        })
    }

    /// Non-blocking drain of pending tray events.
    pub fn drain_events(&self) -> Vec<TrayEvent> {
        let mut out = Vec::new();
        while let Ok(event) = self.events.try_recv() {
            out.push(event);
        }
        out
    }
}

/// Hide the main window into the tray (native).
pub fn hide_window(window: &gpui_kit::Window) {
    window_visibility::set(window, false);
}

/// Bring the main window back (native).
pub fn show_window(window: &gpui_kit::Window) {
    window_visibility::set(window, true);
}

/// Give the tray icon the app icon (taskbar/titlebar icon on Windows; no-op
/// elsewhere where the icon comes from the bundle).
pub fn set_window_icon(window: &gpui_kit::Window) {
    window_icon::set(window);
}

/// Quit the whole process: ask the tray to remove its icon, then exit.
pub fn quit_app() {
    tray::cleanup();
    std::process::exit(0);
}

#[cfg(target_os = "windows")]
mod tray {
    use super::TrayEvent;
    use std::sync::{mpsc, Mutex, OnceLock};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NOTIFYICONDATAW, NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMessageW, HMENU, IMAGE_ICON, LoadImageW,
        LR_DEFAULTSIZE, MF_STRING, MSG, PostMessageW, PostQuitMessage, RegisterClassExW,
        TrackPopupMenu, TranslateMessage, WNDCLASSEXW, WM_APP, WM_COMMAND,
        WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP, WM_RBUTTONUP,
        TPM_LEFTALIGN, TPM_RIGHTBUTTON, HWND_MESSAGE,
    };

    const TRAY_MSG: u32 = WM_APP + 0x100;
    const TRAY_QUIT_CMD: usize = 0xDEAD;
    const MENU_SHOW: usize = 1001;
    const MENU_QUIT: usize = 1002;
    const TRAY_ID: usize = 1;

    /// Null-terminated UTF-16 copy of `s`, kept alive on the thread stack.
    fn utf16(s: &str) -> Vec<u16> {
        s.encode_utf16().chain(std::iter::once(0)).collect()
    }

    static TRAY_TX: OnceLock<Mutex<Option<mpsc::Sender<TrayEvent>>>> = OnceLock::new();
    /// The tray window handle (as an isize so the static is Send/Sync).
    static TRAY_HWND: OnceLock<isize> = OnceLock::new();
    /// The NOTIFYICONDATA registered with the shell, so a quit can remove it.
    static TRAY_NID: OnceLock<SendNid> = OnceLock::new();
    /// One-shot channel used to wait for the tray thread to remove its icon.
    static TRAY_SHUTDOWN_TX: OnceLock<Mutex<Option<mpsc::Sender<()>>>> = OnceLock::new();

    /// NOTIFYICONDATAW holds raw HWND/HICON pointers; wrap it for the static.
    struct SendNid(NOTIFYICONDATAW);
    // The value is only ever read/written within the tray thread (and read at
    // shutdown before the process exits), so the pointers stay valid.
    unsafe impl Send for SendNid {}
    unsafe impl Sync for SendNid {}

    fn sender() -> Option<mpsc::Sender<TrayEvent>> {
        TRAY_TX.get().and_then(|lock| lock.lock().ok()).and_then(|guard| guard.clone())
    }

    fn send(event: TrayEvent) {
        if let Some(tx) = sender() {
            let _ = tx.send(event);
        }
    }

    unsafe extern "system" fn tray_wnd_proc(
        hwnd: HWND,
        msg: u32,
        wparam: WPARAM,
        lparam: LPARAM,
    ) -> LRESULT {
        if msg == TRAY_MSG {
            if lparam as usize == TRAY_QUIT_CMD {
                // App requested quit: remove the tray icon and leave the loop.
                unsafe {
                    if let Some(nid) = TRAY_NID.get() {
                        Shell_NotifyIconW(NIM_DELETE, &nid.0);
                    }
                    PostQuitMessage(0);
                    if let Some(tx) = TRAY_SHUTDOWN_TX.get()
                        && let Ok(guard) = tx.lock()
                        && let Some(tx) = guard.as_ref()
                    {
                        let _ = tx.send(());
                    }
                }
                return 0;
            }
            match lparam as u32 {
                WM_LBUTTONUP | WM_LBUTTONDBLCLK => {
                    send(TrayEvent::Show);
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
        } else if msg == WM_COMMAND {
            match (wparam as usize) & 0xffff {
                MENU_SHOW => send(TrayEvent::Show),
                MENU_QUIT => send(TrayEvent::Quit),
                _ => {}
            }
            return 0;
        } else if msg == WM_DESTROY {
            unsafe {
                PostQuitMessage(0);
            }
            return 0;
        }
        unsafe { DefWindowProcW(hwnd, msg, wparam, lparam) }
    }

    unsafe fn show_menu(hwnd: HWND, x: i32, y: i32) {
        unsafe {
            let menu: HMENU = CreatePopupMenu();
            let show = utf16("打开主界面");
            let quit = utf16("退出Qcode");
            AppendMenuW(menu, MF_STRING, MENU_SHOW, show.as_ptr());
            AppendMenuW(menu, MF_STRING, MENU_QUIT, quit.as_ptr());
            // Screen coordinates of the cursor, so the menu opens by the icon.
            TrackPopupMenu(
                menu,
                TPM_LEFTALIGN | TPM_RIGHTBUTTON,
                x,
                y,
                0,
                hwnd,
                std::ptr::null_mut(),
            );
            DestroyMenu(menu);
        }
    }

    pub struct Platform;

    pub fn install(tx: mpsc::Sender<TrayEvent>) -> Result<Platform, String> {
        TRAY_TX.set(Mutex::new(Some(tx))).map_err(|_| "tray already installed")?;
        std::thread::Builder::new()
            .name("qcode-tray".into())
            .spawn(tray_thread)
            .map_err(|error| error.to_string())?;
        Ok(Platform)
    }

    /// Ask the tray thread to remove its icon; returns once it did (or the
    /// wait times out). Called right before the process exits.
    pub fn cleanup() {
        if let Some(hwnd) = TRAY_HWND.get() {
            let (tx, rx) = mpsc::channel();
            let _ = TRAY_SHUTDOWN_TX.set(Mutex::new(Some(tx)));
            unsafe {
                PostMessageW(*hwnd as HWND, TRAY_MSG, 0, TRAY_QUIT_CMD as isize);
            }
            let _ = rx.recv_timeout(std::time::Duration::from_millis(500));
        }
    }

    fn tray_thread() {
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
                return;
            }
            let _ = TRAY_HWND.set(hwnd as isize);
            // Load the application icon (resource 101, the same logo.ico
            // embedded into qc.exe) scaled to the tray size.
            let hicon = LoadImageW(
                instance,
                101 as *const u16,
                IMAGE_ICON,
                16,
                16,
                LR_DEFAULTSIZE,
            );
            let mut nid: NOTIFYICONDATAW = std::mem::zeroed();
            nid.cbSize = std::mem::size_of::<NOTIFYICONDATAW>() as u32;
            nid.hWnd = hwnd;
            nid.uID = TRAY_ID as u32;
            nid.uFlags = NIF_ICON | NIF_MESSAGE | NIF_TIP;
            nid.uCallbackMessage = TRAY_MSG;
            nid.hIcon = hicon;
            // szTip is the 128-wide array; write the UTF-16 tip.
            let tip = utf16("qcode");
            let tip_len = tip.len().min(nid.szTip.len() - 1);
            nid.szTip[..tip_len].copy_from_slice(&tip[..tip_len]);
            let _ = TRAY_NID.set(SendNid(nid));
            Shell_NotifyIconW(NIM_ADD, &nid);

            let mut msg = MSG::default();
            while GetMessageW(&mut msg, std::ptr::null_mut(), 0, 0) > 0 {
                TranslateMessage(&msg);
                DispatchMessageW(&msg);
            }
            // The loop only ends on WM_DESTROY (process teardown).
        }
    }
}

#[cfg(target_os = "windows")]
mod window_visibility {
    use windows_sys::Win32::UI::WindowsAndMessaging::{ShowWindow, SW_HIDE, SW_SHOW};

    pub fn set(window: &gpui_kit::Window, visible: bool) {
        // gpui's `Window` has an inherent `window_handle()` returning
        // AnyWindowHandle, so the raw-window-handle trait must be called via
        // UFCS to get the native HWND.
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
            ShowWindow(hwnd, if visible { SW_SHOW } else { SW_HIDE });
        }
    }
}

#[cfg(target_os = "macos")]
mod tray {
    use super::TrayEvent;
    use std::sync::{mpsc, Mutex, OnceLock};
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, Sel};
    use objc2::{declare_class, msg_send, sel, ClassType};
    use objc2_app_kit::{
        NSMenu, NSMenuItem, NSStatusBar, NSStatusItem, NSStatusItemLength,
    };
    use objc2_foundation::NSString;

    static TRAY_TX: OnceLock<Mutex<Option<mpsc::Sender<TrayEvent>>>> = OnceLock::new();

    /// A minimal NSObject that receives the tray menu actions.
    declare_class! {
        struct TrayTarget;

        unsafe impl ClassType for TrayTarget {
            type Super = NSObject;
        }

        unsafe impl TrayTarget {
            #[method(showAction:)]
            unsafe fn show(&self, _: Sel) {
                send(TrayEvent::Show);
            }
            #[method(quitAction:)]
            unsafe fn quit(&self, _: Sel) {
                send(TrayEvent::Quit);
            }
        }
    }

    /// Keeps the status item and its action target alive for the process.
    pub struct Platform {
        #[allow(dead_code)]
        item: Retained<NSStatusItem>,
        #[allow(dead_code)]
        target: Retained<TrayTarget>,
    }

    pub fn install(tx: mpsc::Sender<TrayEvent>) -> Result<Platform, String> {
        TRAY_TX.set(Mutex::new(Some(tx))).map_err(|_| "tray already installed")?;
        let (item, target) = create_status_item()?;
        Ok(Platform { item, target })
    }

    /// No explicit cleanup needed on macOS: the status item disappears with
    /// the process.
    pub fn cleanup() {}

    fn send(event: TrayEvent) {
        if let Some(lock) = TRAY_TX.get()
            && let Ok(guard) = lock.lock()
            && let Some(tx) = guard.as_ref()
        {
            let _ = tx.send(event);
        }
    }

    fn create_status_item() -> Result<(Retained<NSStatusItem>, Retained<TrayTarget>), String> {
        unsafe {
            let target = TrayTarget::new();
            let bar = NSStatusBar::systemStatusBar();
            let item: Retained<NSStatusItem> = msg_send![
                &bar,
                statusItemWithLength: NSStatusItemLength(30.0)
            ];
            let button: Retained<objc2_app_kit::NSStatusBarButton> = msg_send![&item, button];
            let title = NSString::from_str("qcode");
            let _: () = msg_send![&button, setTitle: &title];
            let _: () = msg_send![&button, setTarget: Some(&*target)];
            let _: () = msg_send![&button, setAction: sel!(showAction:)];

            let menu = NSMenu::new();
            let show = NSMenuItem::alloc();
            let show_title = NSString::from_str("打开主界面");
            let _: () = msg_send![
                &show,
                initWithTitle: &show_title
                    action: sel!(showAction:)
                    keyEquivalent: NSString::from_str("")
            ];
            let quit = NSMenuItem::alloc();
            let quit_title = NSString::from_str("退出Qcode");
            let _: () = msg_send![
                &quit,
                initWithTitle: &quit_title
                    action: sel!(quitAction:)
                    keyEquivalent: NSString::from_str("")
            ];
            let _: () = msg_send![&menu, addItem: &show];
            let _: () = msg_send![&menu, addItem: &quit];
            let _: () = msg_send![&item, setMenu: Some(&menu)];
            Ok((item, target))
        }
    }
}

#[cfg(target_os = "macos")]
mod window_visibility {
    pub fn set(window: &gpui_kit::Window, visible: bool) {
        let Ok(handle) =
            <gpui_kit::Window as raw_window_handle::HasWindowHandle>::window_handle(window)
        else {
            return;
        };
        let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
            return;
        };
        unsafe {
            let ns_window = appkit.ns_window as *mut objc2::runtime::AnyObject;
            if !ns_window.is_null() {
                if visible {
                    let _: () = objc2::msg_send![ns_window, makeKeyAndOrderFront: std::ptr::null_mut::<objc2::runtime::AnyObject>()];
                } else {
                    let _: () = objc2::msg_send![ns_window, orderOut: std::ptr::null_mut::<objc2::runtime::AnyObject>()];
                }
            }
        }
    }
}

#[cfg(target_os = "linux")]
mod tray {
    use super::TrayEvent;
    use std::sync::{mpsc, Mutex, OnceLock};
    use zbus::{connection, interface, Connection};

    static TRAY_TX: OnceLock<Mutex<Option<mpsc::Sender<TrayEvent>>>> = OnceLock::new();

    #[interface(name = "org.kde.StatusNotifierItem")]
    pub struct Sni {
        #[zbus(signal)]
        _signal: (),
    }

    impl Sni {
        #[zbus(property)]
        fn title(&self) -> String {
            "qcode".into()
        }
        #[zbus(property)]
        fn status(&self) -> String {
            "Active".into()
        }
        #[zbus(property)]
        fn icon_name(&self) -> String {
            "qcode".into()
        }
        fn activate(&self, _x: i32, _y: i32) {
            if let Some(tx) = sender() {
                let _ = tx.send(TrayEvent::Show);
            }
        }
        fn context_menu(&self, _x: i32, _y: i32) {}
        fn secondary_activate(&self, _x: i32, _y: i32) {}
    }

    fn sender() -> Option<mpsc::Sender<TrayEvent>> {
        TRAY_TX.get().and_then(|lock| lock.lock().ok()).and_then(|g| g.clone())
    }

    pub struct Platform {
        #[allow(dead_code)]
        conn: Connection,
    }

    pub fn install(tx: mpsc::Sender<TrayEvent>) -> Result<Platform, String> {
        TRAY_TX.set(Mutex::new(Some(tx))).map_err(|_| "tray already installed")?;
        let conn = connection::Builder::session()
            .name("org.kde.StatusNotifierItem-qcode-1")
            .build()
            .map_err(|error| error.to_string())?;
        let _ = conn.object_server().at(
            "/StatusNotifierItem",
            Sni { _signal: () },
        );
        Ok(Platform { conn })
    }

    /// No explicit cleanup needed: the DBus connection drops with the process.
    pub fn cleanup() {}
}

#[cfg(target_os = "linux")]
mod window_visibility {
    pub fn set(window: &gpui_kit::Window, visible: bool) {
        // There is no portable way to unmap a Wayland surface from the app,
        // and X11 unmapping would pull in an x11 dependency, so hide by
        // minimizing instead: the window leaves the workspace and the tray
        // icon keeps the process resident.
        if visible {
            window.activate_window();
        } else {
            window.minimize_window();
        }
    }
}

/// Sets the main window's taskbar/titlebar icon (Windows only; elsewhere the
/// icon comes from the bundle).
#[cfg(target_os = "windows")]
mod window_icon {
    use windows_sys::Win32::Foundation::LPARAM;
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        LoadImageW, SendMessageW, WM_SETICON, ICON_BIG, ICON_SMALL, IMAGE_ICON, LR_DEFAULTSIZE,
    };

    pub fn set(window: &gpui_kit::Window) {
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
            let instance = GetModuleHandleW(std::ptr::null());
            // Resource 101 is the logo.ico embedded into qc.exe.
            let big = LoadImageW(instance, 101 as *const u16, IMAGE_ICON, 32, 32, LR_DEFAULTSIZE);
            let small =
                LoadImageW(instance, 101 as *const u16, IMAGE_ICON, 16, 16, LR_DEFAULTSIZE);
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as LPARAM);
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as LPARAM);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod window_icon {
    pub fn set(_window: &gpui_kit::Window) {}
}
