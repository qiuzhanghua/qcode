//! Close-to-tray: a system tray icon (Windows notification area, macOS menu
//! bar, Linux StatusNotifier) plus native window hiding. Closing the main
//! window hides it and keeps the process resident behind the tray icon, the
//! way chat apps like QQ do; the tray offers a show/hide toggle and quit.

use std::sync::atomic::{AtomicBool, Ordering};
use std::sync::mpsc;

/// Whether a tray icon is present (tray install succeeded). Read from the
/// window's close callback, which cannot reach into the Shell state.
static TRAY_ACTIVE: AtomicBool = AtomicBool::new(false);

/// Whether the tray icon is installed and close-to-tray should apply.
pub fn is_active() -> bool {
    TRAY_ACTIVE.load(Ordering::Relaxed)
}

/// Whether the main window is currently hidden behind the tray. Every hide and
/// show path funnels through `hide_window` / `show_window`, so this stays in
/// step with what the user sees; it is the single source of truth for the
/// tray's show/hide toggle.
static WINDOW_HIDDEN: AtomicBool = AtomicBool::new(false);

/// Whether the main window is hidden (close-to-tray or the tray toggle).
pub fn is_window_hidden() -> bool {
    WINDOW_HIDDEN.load(Ordering::Relaxed)
}

/// What the tray icon asks the app to do.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum TrayEvent {
    /// Left-click / "show" menu item: bring the main window back.
    Show,
    /// "显示/隐藏面板" menu item: hide a shown window, show a hidden one.
    Toggle,
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
    WINDOW_HIDDEN.store(true, Ordering::Relaxed);
    window_visibility::set(window, false);
}

/// Bring the main window back (native).
pub fn show_window(window: &gpui_kit::Window) {
    WINDOW_HIDDEN.store(false, Ordering::Relaxed);
    window_visibility::set(window, true);
}

/// Show a hidden window, hide a shown one. Used by the tray's single
/// "显示/隐藏面板" menu item, so the label never has to decide the action.
pub fn toggle_window(window: &gpui_kit::Window) {
    if is_window_hidden() {
        show_window(window);
        window.activate_window();
    } else {
        hide_window(window);
    }
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
    use std::sync::{Mutex, OnceLock, mpsc};
    use windows_sys::Win32::Foundation::{HWND, LPARAM, LRESULT, POINT, WPARAM};
    use windows_sys::Win32::System::LibraryLoader::GetModuleHandleW;
    use windows_sys::Win32::UI::Shell::{
        NIF_ICON, NIF_MESSAGE, NIF_TIP, NIM_ADD, NIM_DELETE, NOTIFYICONDATAW, Shell_NotifyIconW,
    };
    use windows_sys::Win32::UI::WindowsAndMessaging::{
        AppendMenuW, CreatePopupMenu, CreateWindowExW, DefWindowProcW, DestroyMenu,
        DispatchMessageW, GetCursorPos, GetMessageW, GetSystemMetrics, HMENU, HWND_MESSAGE,
        IMAGE_ICON, LoadImageW, MF_SEPARATOR, MF_STRING, MSG, PostMessageW, PostQuitMessage,
        RegisterClassExW, SM_CXSMICON, SM_CYSMICON, TPM_LEFTALIGN, TPM_RIGHTBUTTON, TrackPopupMenu,
        TranslateMessage, WM_APP, WM_COMMAND, WM_DESTROY, WM_LBUTTONDBLCLK, WM_LBUTTONUP,
        WM_RBUTTONUP, WNDCLASSEXW,
    };

    const TRAY_MSG: u32 = WM_APP + 0x100;
    const TRAY_QUIT_CMD: usize = 0xDEAD;
    const MENU_TOGGLE: usize = 1001;
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
        TRAY_TX
            .get()
            .and_then(|lock| lock.lock().ok())
            .and_then(|guard| guard.clone())
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
                MENU_TOGGLE => send(TrayEvent::Toggle),
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
            let toggle = utf16("显示/隐藏面板");
            let quit = utf16("退出 qcode");
            AppendMenuW(menu, MF_STRING, MENU_TOGGLE, toggle.as_ptr());
            AppendMenuW(menu, MF_SEPARATOR, 0, std::ptr::null());
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
        TRAY_TX
            .set(Mutex::new(Some(tx)))
            .map_err(|_| "tray already installed")?;
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
            // Resource 102 is the tray mark (brand blue); 101 is the app icon.
            // Load at the shell's small-icon metric so 125%/150% scaling does
            // not stretch a 16 px bitmap.
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
    use windows_sys::Win32::UI::WindowsAndMessaging::{SW_HIDE, SW_SHOW, ShowWindow};

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
    use objc2::rc::Retained;
    use objc2::runtime::{AnyObject, NSObject, Sel};
    use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
    use objc2_app_kit::{
        NSEventModifierFlags, NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusItem,
        NSVariableStatusItemLength,
    };
    use objc2_foundation::{NSData, NSSize, NSString};
    use std::sync::{Mutex, OnceLock, mpsc};

    /// Menu-bar template (black + alpha), embedded so the tray works both in a
    /// bundled .app and when the binary runs straight out of target/.
    ///
    /// A template image is tinted by the system for light/dark menu bars; the
    /// vector PDF keeps it crisp at every scale.
    const TEMPLATE_PDF: &[u8] = include_bytes!("../../qcode/res/icons/macos/qcodeTemplate.pdf");

    static TRAY_TX: OnceLock<Mutex<Option<mpsc::Sender<TrayEvent>>>> = OnceLock::new();

    // A minimal NSObject that receives the tray menu actions.
    define_class!(
        // SAFETY: NSObject has no subclassing requirements, and TrayTarget has
        // no instance variables and no `Drop` implementation.
        #[unsafe(super(NSObject))]
        #[name = "QcodeTrayTarget"]
        struct TrayTarget;

        impl TrayTarget {
            #[unsafe(method(toggleAction:))]
            fn toggle(&self, _: Sel) {
                send(TrayEvent::Toggle);
            }

            #[unsafe(method(quitAction:))]
            fn quit(&self, _: Sel) {
                send(TrayEvent::Quit);
            }
        }
    );

    impl TrayTarget {
        fn new() -> Retained<Self> {
            let this = Self::alloc().set_ivars(());
            unsafe { msg_send![super(this), init] }
        }
    }

    /// Any Objective-C object is a valid `AnyObject`; the cast is needed
    /// because `NSMenuItem::setTarget` takes a bare `&AnyObject`.
    fn as_any_object<T>(object: &T) -> &AnyObject {
        // SAFETY: every Objective-C object starts with its `isa`, and
        // `AnyObject` is a zero-sized view of exactly that.
        unsafe { &*(object as *const T as *const AnyObject) }
    }

    /// Build the menu-bar image from the embedded template PDF. The image is
    /// sized for the 22 pt menu-bar working area; @2x is handled by the vector.
    fn template_image() -> Option<Retained<NSImage>> {
        let data = NSData::with_bytes(TEMPLATE_PDF);
        let image = NSImage::initWithData(NSImage::alloc(), &data)?;
        image.setSize(NSSize::new(18.0, 18.0));
        image.setTemplate(true);
        Some(image)
    }

    /// Keeps the status item and its action target alive for the process.
    pub struct Platform {
        #[allow(dead_code)]
        item: Retained<NSStatusItem>,
        #[allow(dead_code)]
        target: Retained<TrayTarget>,
    }

    pub fn install(tx: mpsc::Sender<TrayEvent>) -> Result<Platform, String> {
        TRAY_TX
            .set(Mutex::new(Some(tx)))
            .map_err(|_| "tray already installed")?;
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
        // AppKit status items and menus must be created on the main thread.
        let mtm = MainThreadMarker::new()
            .ok_or_else(|| "tray must be created on the main thread".to_string())?;
        let target = TrayTarget::new();
        unsafe {
            let bar = NSStatusBar::systemStatusBar();
            let item = bar.statusItemWithLength(NSVariableStatusItemLength);

            if let Some(button) = item.button(mtm) {
                match template_image() {
                    Some(image) => button.setImage(Some(&image)),
                    // Losing the template is not fatal; fall back to the name.
                    None => button.setTitle(&NSString::from_str("qcode")),
                }
                button.setToolTip(Some(&NSString::from_str("qcode")));
            }

            let menu = NSMenu::new(mtm);
            // Enable the items ourselves; the target is not in the responder
            // chain, so AppKit's automatic validation would gray them out.
            menu.setAutoenablesItems(false);

            let toggle = NSMenuItem::new(mtm);
            toggle.setTitle(&NSString::from_str("显示/隐藏面板"));
            toggle.setAction(Some(sel!(toggleAction:)));
            toggle.setTarget(Some(as_any_object(&*target)));

            let quit = NSMenuItem::new(mtm);
            quit.setTitle(&NSString::from_str("退出 qcode"));
            quit.setAction(Some(sel!(quitAction:)));
            quit.setTarget(Some(as_any_object(&*target)));
            quit.setKeyEquivalent(&NSString::from_str("q"));
            quit.setKeyEquivalentModifierMask(NSEventModifierFlags::Command);

            menu.addItem(&toggle);
            menu.addItem(&NSMenuItem::separatorItem(mtm));
            menu.addItem(&quit);
            item.setMenu(Some(&menu));
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
            // raw-window-handle exposes the NSView, not the NSWindow.
            let ns_view = appkit.ns_view.as_ptr() as *mut objc2::runtime::AnyObject;
            if ns_view.is_null() {
                return;
            }
            let ns_window: *mut objc2::runtime::AnyObject = objc2::msg_send![ns_view, window];
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
    use std::collections::HashMap;
    use std::sync::{Mutex, OnceLock, mpsc};
    use zbus::blocking::connection::Builder;
    use zbus::interface;
    use zbus::zvariant::{OwnedObjectPath, OwnedValue, StructureBuilder, Type, Value};

    static TRAY_TX: OnceLock<Mutex<Option<mpsc::Sender<TrayEvent>>>> = OnceLock::new();
    /// IconPixmap payload: (width, height, ARGB32 in network byte order).
    static ICON_PIXMAPS: OnceLock<Vec<(i32, i32, Vec<u8>)>> = OnceLock::new();

    /// The tray mark at the sizes panels ask for. Embedded so the icon shows
    /// up even when no hicolor theme is installed next to the binary.
    const ICONS: &[&[u8]] = &[
        include_bytes!("../../qcode/res/icons/linux/hicolor/16x16/apps/qcode.png"),
        include_bytes!("../../qcode/res/icons/linux/hicolor/22x22/apps/qcode.png"),
        include_bytes!("../../qcode/res/icons/linux/hicolor/32x32/apps/qcode.png"),
        include_bytes!("../../qcode/res/icons/linux/hicolor/48x48/apps/qcode.png"),
    ];

    pub struct Sni;

    #[interface(name = "org.kde.StatusNotifierItem")]
    impl Sni {
        #[zbus(property)]
        fn title(&self) -> String {
            "qcode".into()
        }
        #[zbus(property)]
        fn status(&self) -> String {
            "Active".into()
        }
        /// Empty on purpose: hosts should use IconPixmap, because the binary
        /// does not install itself into an icon theme.
        #[zbus(property)]
        fn icon_name(&self) -> String {
            String::new()
        }
        #[zbus(property)]
        fn icon_pixmap(&self) -> Vec<(i32, i32, Vec<u8>)> {
            ICON_PIXMAPS.get().cloned().unwrap_or_default()
        }
        /// Right-click menu, served by the dbusmenu object below.
        #[zbus(property)]
        fn menu(&self) -> OwnedObjectPath {
            OwnedObjectPath::try_from(MENU_PATH).expect("static menu path is valid")
        }
        fn activate(&self, _x: i32, _y: i32) {
            if let Some(tx) = sender() {
                let _ = tx.send(TrayEvent::Show);
            }
        }
        fn context_menu(&self, _x: i32, _y: i32) {}
        fn secondary_activate(&self, _x: i32, _y: i32) {}
    }

    // --- com.canonical.dbusmenu -------------------------------------------
    //
    // `GetLayout` returns a recursive `(ia{sv}av)`: each child is a variant
    // wrapping another node of the same shape. LayoutNode encodes that
    // signature, and the children are built by hand because a derived
    // `Serialize` offers no way *into* a `Value`.

    const MENU_PATH: &str = "/MenuBar";
    const MENU_ROOT: i32 = 0;
    const MENU_TOGGLE: i32 = 1;
    const MENU_SEPARATOR: i32 = 2;
    const MENU_QUIT: i32 = 3;
    const MENU_REVISION: u32 = 1;

    type MenuProps = HashMap<String, OwnedValue>;

    #[derive(Debug, Clone, serde::Serialize, Type)]
    struct LayoutNode(i32, MenuProps, Vec<OwnedValue>);

    fn menu_text(value: &str) -> OwnedValue {
        OwnedValue::try_from(Value::from(value)).expect("a string is a value")
    }

    fn menu_bool(value: bool) -> OwnedValue {
        OwnedValue::try_from(Value::from(value)).expect("a bool is a value")
    }

    fn item_props(label: &str) -> MenuProps {
        let mut props = MenuProps::new();
        props.insert("label".into(), menu_text(label));
        props.insert("enabled".into(), menu_bool(true));
        props.insert("visible".into(), menu_bool(true));
        props
    }

    fn separator_props() -> MenuProps {
        let mut props = MenuProps::new();
        props.insert("type".into(), menu_text("separator"));
        props
    }

    fn root_props() -> MenuProps {
        let mut props = MenuProps::new();
        // Tells the host this row opens a submenu instead of acting as a leaf.
        props.insert("children-display".into(), menu_text("submenu"));
        props
    }

    fn row_props(id: i32) -> Option<MenuProps> {
        match id {
            MENU_TOGGLE => Some(item_props("显示/隐藏面板")),
            MENU_SEPARATOR => Some(separator_props()),
            MENU_QUIT => Some(item_props("退出 qcode")),
            _ => None,
        }
    }

    fn into_value(node: LayoutNode) -> OwnedValue {
        let structure = StructureBuilder::new()
            .add_field(node.0)
            .add_field(node.1)
            .add_field(node.2)
            .build()
            .expect("a layout node has three fields");
        OwnedValue::try_from(Value::from(structure)).expect("a structure is a value")
    }

    fn row(id: i32, props: MenuProps) -> OwnedValue {
        into_value(LayoutNode(id, props, Vec::new()))
    }

    /// Build the top-level tree; `depth == 0` keeps only the root's own rows.
    fn tree(depth: i32) -> LayoutNode {
        let children = if depth == 0 {
            Vec::new()
        } else {
            vec![
                row(MENU_TOGGLE, item_props("显示/隐藏面板")),
                row(MENU_SEPARATOR, separator_props()),
                row(MENU_QUIT, item_props("退出 qcode")),
            ]
        };
        LayoutNode(MENU_ROOT, root_props(), children)
    }

    fn handle_menu_event(id: i32) {
        let event = match id {
            MENU_TOGGLE => TrayEvent::Toggle,
            MENU_QUIT => TrayEvent::Quit,
            _ => return,
        };
        if let Some(tx) = sender() {
            let _ = tx.send(event);
        }
    }

    struct MenuBar;

    #[interface(name = "com.canonical.dbusmenu")]
    impl MenuBar {
        #[zbus(property)]
        fn version(&self) -> u32 {
            3
        }
        #[zbus(property)]
        fn status(&self) -> String {
            "normal".into()
        }
        #[zbus(property)]
        fn text_direction(&self) -> String {
            "ltr".into()
        }
        #[zbus(property)]
        fn icon_theme_path(&self) -> Vec<String> {
            Vec::new()
        }

        fn get_layout(
            &self,
            parent_id: i32,
            recursion_depth: i32,
            _property_names: Vec<String>,
        ) -> (u32, LayoutNode) {
            if parent_id == MENU_ROOT {
                return (MENU_REVISION, tree(recursion_depth));
            }
            match row_props(parent_id) {
                Some(props) => (MENU_REVISION, LayoutNode(parent_id, props, Vec::new())),
                // Stale ids are answered with an empty node, not an error.
                None => (
                    MENU_REVISION,
                    LayoutNode(parent_id, MenuProps::new(), Vec::new()),
                ),
            }
        }

        fn get_group_properties(
            &self,
            ids: Vec<i32>,
            _property_names: Vec<String>,
        ) -> Vec<(i32, MenuProps)> {
            let ids = if ids.is_empty() {
                vec![MENU_ROOT, MENU_TOGGLE, MENU_SEPARATOR, MENU_QUIT]
            } else {
                ids
            };
            ids.into_iter()
                .map(|id| {
                    let props = if id == MENU_ROOT {
                        root_props()
                    } else {
                        row_props(id).unwrap_or_default()
                    };
                    (id, props)
                })
                .collect()
        }

        fn get_property(&self, id: i32, name: String) -> OwnedValue {
            let props = if id == MENU_ROOT {
                root_props()
            } else {
                row_props(id).unwrap_or_default()
            };
            props.get(&name).cloned().unwrap_or_else(|| menu_text(""))
        }

        fn about_to_show(&self, _id: i32) -> bool {
            false
        }

        fn about_to_show_group(&self, _ids: Vec<i32>) -> (Vec<i32>, Vec<i32>) {
            (Vec::new(), Vec::new())
        }

        fn event(&self, id: i32, event_id: String, _data: Value<'_>, _timestamp: u32) {
            if event_id == "clicked" {
                handle_menu_event(id);
            }
        }

        /// Hosts batch events; answering only `Event` leaves menus that use
        /// this one unclickable.
        fn event_group(&self, events: Vec<(i32, String, Value<'_>, u32)>) -> Vec<i32> {
            for (id, event_id, data, timestamp) in events {
                self.event(id, event_id, data, timestamp);
            }
            Vec::new()
        }

        #[zbus(signal)]
        async fn layout_updated(
            emitter: &zbus::object_server::SignalEmitter<'_>,
            revision: u32,
            parent: i32,
        ) -> zbus::Result<()>;
    }

    fn sender() -> Option<mpsc::Sender<TrayEvent>> {
        TRAY_TX
            .get()
            .and_then(|lock| lock.lock().ok())
            .and_then(|g| g.clone())
    }

    pub struct Platform {
        #[allow(dead_code)]
        conn: zbus::blocking::Connection,
    }

    /// Decode the embedded PNGs into the ARGB32 pixmaps the SNI spec wants.
    fn argb_pixmaps() -> Vec<(i32, i32, Vec<u8>)> {
        let mut pixmaps = Vec::new();
        for bytes in ICONS {
            let mut decoder = png::Decoder::new(std::io::Cursor::new(*bytes));
            // Palette/16-bit inputs become plain 8-bit RGB(A).
            decoder.set_transformations(png::Transformations::normalize_to_color8());
            let Ok(mut reader) = decoder.read_info() else {
                continue;
            };
            let Some(size) = reader.output_buffer_size() else {
                continue;
            };
            let mut buffer = vec![0u8; size];
            let Ok(info) = reader.next_frame(&mut buffer) else {
                continue;
            };
            let data = &buffer[..info.buffer_size()];
            let (width, height) = (info.width as i32, info.height as i32);
            let mut argb = Vec::with_capacity((width * height * 4) as usize);
            match info.color_type {
                png::ColorType::Rgba => {
                    for px in data.chunks_exact(4) {
                        argb.extend_from_slice(&[px[3], px[0], px[1], px[2]]);
                    }
                }
                png::ColorType::Rgb => {
                    for px in data.chunks_exact(3) {
                        argb.extend_from_slice(&[0xff, px[0], px[1], px[2]]);
                    }
                }
                _ => continue,
            }
            pixmaps.push((width, height, argb));
        }
        pixmaps
    }

    pub fn install(tx: mpsc::Sender<TrayEvent>) -> Result<Platform, String> {
        TRAY_TX
            .set(Mutex::new(Some(tx)))
            .map_err(|_| "tray already installed")?;
        let _ = ICON_PIXMAPS.set(argb_pixmaps());
        // Blocking API: the tray lives on its own thread, so no async runtime
        // has to be started just to publish the status item.
        // The spec wants org.kde.StatusNotifierItem-<pid>-<n>; the watcher
        // parses the PID out of the name.
        let service = format!("org.kde.StatusNotifierItem-{}-1", std::process::id());
        let conn = Builder::session()
            .map_err(|error| error.to_string())?
            .name(service.clone())
            .map_err(|error| error.to_string())?
            .build()
            .map_err(|error| error.to_string())?;
        conn.object_server()
            .at("/StatusNotifierItem", Sni)
            .map_err(|error| error.to_string())?;
        conn.object_server()
            .at(MENU_PATH, MenuBar)
            .map_err(|error| error.to_string())?;
        // Without this call the item is on the bus but no host ever shows it.
        // A missing watcher (e.g. stock GNOME) is not fatal.
        let _ = conn.call_method(
            Some("org.kde.StatusNotifierWatcher"),
            "/StatusNotifierWatcher",
            Some("org.kde.StatusNotifierWatcher"),
            "RegisterStatusNotifierItem",
            &service,
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
        GetSystemMetrics, ICON_BIG, ICON_SMALL, IMAGE_ICON, LoadImageW, SM_CXICON, SM_CXSMICON,
        SM_CYICON, SM_CYSMICON, SendMessageW, WM_SETICON,
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
            // Resource 101 is the colour app icon embedded into qc.exe; ask
            // for the shell's metrics rather than fixed pixels.
            let big = LoadImageW(
                instance,
                101 as *const u16,
                IMAGE_ICON,
                GetSystemMetrics(SM_CXICON),
                GetSystemMetrics(SM_CYICON),
                0,
            );
            let small = LoadImageW(
                instance,
                101 as *const u16,
                IMAGE_ICON,
                GetSystemMetrics(SM_CXSMICON),
                GetSystemMetrics(SM_CYSMICON),
                0,
            );
            SendMessageW(hwnd, WM_SETICON, ICON_BIG as usize, big as LPARAM);
            SendMessageW(hwnd, WM_SETICON, ICON_SMALL as usize, small as LPARAM);
        }
    }
}

#[cfg(not(target_os = "windows"))]
mod window_icon {
    pub fn set(_window: &gpui_kit::Window) {}
}
