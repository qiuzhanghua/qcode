//! macOS 托盘：NSStatusItem + 菜单（ADR-0052 的 macOS 后端）。
//!
//! 菜单栏项必须在主线程创建；`start()` 由 `run()` 在主线程调用，`update()`
//! 由 GPUI 的 foreground task（同样在主线程）调用，因此这里不需要额外线程。

use std::sync::mpsc::Sender;
use std::sync::{Mutex, OnceLock};

use objc2::rc::Retained;
use objc2::runtime::{AnyObject, NSObject, Sel};
use objc2::{AnyThread, MainThreadMarker, define_class, msg_send, sel};
use objc2_app_kit::{
    NSImage, NSMenu, NSMenuItem, NSStatusBar, NSStatusBarButton, NSStatusItem,
    NSVariableStatusItemLength,
};
use objc2_foundation::{NSData, NSSize, NSString};

use super::state::{TrayCommand, TraySnapshot};

/// 菜单栏模板图（黑 + alpha），系统会按浅色/深色自动着色。
const TEMPLATE_PDF: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../qcode/res/icons/macos/qcodeTemplate.pdf"
));

static COMMANDS: OnceLock<Mutex<Option<Sender<TrayCommand>>>> = OnceLock::new();

fn send(command: TrayCommand) {
    if let Some(lock) = COMMANDS.get()
        && let Ok(guard) = lock.lock()
        && let Some(tx) = guard.as_ref()
    {
        let _ = tx.send(command);
    }
}

define_class!(
    // SAFETY: NSObject 没有子类要求，TrayTarget 也没有实例变量和 Drop。
    #[unsafe(super(NSObject))]
    #[name = "QcodeMacTrayTarget"]
    struct TrayTarget;

    impl TrayTarget {
        #[unsafe(method(showAction:))]
        fn show(&self, _: Sel) {
            send(TrayCommand::ShowWindow);
        }

        #[unsafe(method(newTaskAction:))]
        fn new_task(&self, _: Sel) {
            send(TrayCommand::NewTask);
        }

        #[unsafe(method(stopAction:))]
        fn stop(&self, _: Sel) {
            send(TrayCommand::StopCurrent);
        }

        #[unsafe(method(quitAction:))]
        fn quit(&self, _: Sel) {
            send(TrayCommand::Quit);
        }
    }
);

impl TrayTarget {
    fn new() -> Retained<Self> {
        let this = Self::alloc().set_ivars(());
        unsafe { msg_send![super(this), init] }
    }
}

/// Any Objective-C 对象都可以当作 `AnyObject`；`setTarget` 只收裸的 `&AnyObject`。
fn as_any_object<T>(object: &T) -> &AnyObject {
    // SAFETY: Objective-C 对象都以 isa 开头，AnyObject 是零大小视图。
    unsafe { &*(object as *const T as *const AnyObject) }
}

fn template_image() -> Option<Retained<NSImage>> {
    let data = NSData::with_bytes(TEMPLATE_PDF);
    let image = NSImage::initWithData(NSImage::alloc(), &data)?;
    image.setSize(NSSize::new(18.0, 18.0));
    image.setTemplate(true);
    Some(image)
}

pub struct MacTray {
    _item: Retained<NSStatusItem>,
    _menu: Retained<NSMenu>,
    _target: Retained<TrayTarget>,
    button: Option<Retained<NSStatusBarButton>>,
    status: Retained<NSMenuItem>,
    stop: Retained<NSMenuItem>,
}

impl MacTray {
    pub fn update(&self, snapshot: TraySnapshot) {
        self.status
            .setTitle(&NSString::from_str(&snapshot.status_line()));
        self.stop.setEnabled(snapshot.can_stop());
        if let Some(button) = &self.button {
            button.setToolTip(Some(&NSString::from_str(&snapshot.tooltip())));
        }
    }
}

pub fn start(commands: Sender<TrayCommand>) -> Option<MacTray> {
    let mtm = MainThreadMarker::new()?;
    let _ = COMMANDS.set(Mutex::new(Some(commands)));
    let target = TrayTarget::new();

    let bar = NSStatusBar::systemStatusBar();
    let item = bar.statusItemWithLength(NSVariableStatusItemLength);
    let button = item.button(mtm);
    if let Some(button) = &button {
        match template_image() {
            Some(image) => button.setImage(Some(&image)),
            // 模板图加载失败也不致命，退化成文字。
            None => button.setTitle(&NSString::from_str("QCode")),
        }
        button.setToolTip(Some(&NSString::from_str("QCode")));
    }

    let menu = NSMenu::new(mtm);
    // 菜单项由我们自己控制可用状态（target 不在响应链上，自动校验会全部置灰）。
    menu.setAutoenablesItems(false);

    let status = NSMenuItem::new(mtm);
    status.setTitle(&NSString::from_str("QCode"));
    status.setEnabled(false);

    let mk_item = |title: &str, action: Sel| {
        let entry = NSMenuItem::new(mtm);
        entry.setTitle(&NSString::from_str(title));
        // SAFETY: `action` is one of the selectors implemented on TrayTarget.
        unsafe {
            entry.setAction(Some(action));
            entry.setTarget(Some(as_any_object(&*target)));
        }
        entry
    };
    let show = mk_item("显示主窗口", sel!(showAction:));
    let new_task = mk_item("新建任务", sel!(newTaskAction:));
    let stop = mk_item("停止当前任务", sel!(stopAction:));
    stop.setEnabled(false);
    let quit = mk_item("退出", sel!(quitAction:));
    quit.setKeyEquivalent(&NSString::from_str("q"));
    quit.setKeyEquivalentModifierMask(objc2_app_kit::NSEventModifierFlags::Command);

    for entry in [&status, &NSMenuItem::separatorItem(mtm)] {
        menu.addItem(entry);
    }
    for entry in [&show, &new_task, &stop] {
        menu.addItem(entry);
    }
    menu.addItem(&NSMenuItem::separatorItem(mtm));
    menu.addItem(&quit);
    item.setMenu(Some(&menu));

    Some(MacTray {
        _item: item,
        _menu: menu,
        _target: target,
        button,
        status,
        stop,
    })
}

/// 读 NSWindow 指针并持有一次引用，**不产生任何原生副作用**。
///
/// 推迟到后面的 `orderOut:` 执行时窗口可能已被移除，所以这里 `retain`，
/// 由 [`hide_native`] 负责 `release`。
pub fn capture(window: &gpui_kit::Window) -> Option<usize> {
    let handle =
        <gpui_kit::Window as raw_window_handle::HasWindowHandle>::window_handle(window).ok()?;
    let raw_window_handle::RawWindowHandle::AppKit(appkit) = handle.as_raw() else {
        return None;
    };
    unsafe {
        // raw-window-handle 给的是 NSView，NSWindow 要自己取。
        let ns_view = appkit.ns_view.as_ptr() as *mut AnyObject;
        if ns_view.is_null() {
            return None;
        }
        let ns_window: *mut AnyObject = msg_send![ns_view, window];
        if ns_window.is_null() {
            return None;
        }
        let _: *mut AnyObject = msg_send![ns_window, retain];
        Some(ns_window as usize)
    }
}

/// `orderOut:`：把窗口从屏幕移除（Dock 图标也跟着消失），恢复走
/// `show_window` 的 `makeKeyAndOrderFront:`。同时归还 [`capture`] 的引用。
pub fn hide_native(ns_window: usize) {
    unsafe {
        let ns_window = ns_window as *mut AnyObject;
        let _: () = msg_send![ns_window, orderOut: std::ptr::null_mut::<AnyObject>()];
        let _: () = msg_send![ns_window, release];
    }
}

/// 隐藏主窗口：关闭按钮 = 最小化到托盘（ADR-0052）。`orderOut:` 把窗口
/// 从屏幕移除（Dock 图标也跟着消失），恢复走 `show_window` 的
/// `makeKeyAndOrderFront:`。
pub fn hide_window(window: &gpui_kit::Window) {
    if let Some(ns_window) = capture(window) {
        hide_native(ns_window);
    }
}

/// 从最小化/隐藏恢复到前台。`activate_window()` 只激活不还原，所以先 deminiaturize。
///
/// **只能在没有 App 可变借用时调用**，否则 `makeKeyAndOrderFront:` 同步派发的
/// `windowDidBecomeKey:` 会二次借用 App（见 [`super::hide_window_later`]）。
pub fn show_window(window: &gpui_kit::Window) {
    if let Some(ns_window) = capture(window) {
        show_native(ns_window);
    }
    window.activate_window();
}

/// `deminiaturize:` + `makeKeyAndOrderFront:`，并归还 [`capture`] 的引用。
pub fn show_native(ns_window: usize) {
    unsafe {
        let ns_window = ns_window as *mut AnyObject;
        let minimized: bool = msg_send![ns_window, isMiniaturized];
        if minimized {
            let _: () = msg_send![ns_window, deminiaturize: std::ptr::null_mut::<AnyObject>()];
        }
        let _: () = msg_send![
            ns_window,
            makeKeyAndOrderFront: std::ptr::null_mut::<AnyObject>()
        ];
        let _: () = msg_send![ns_window, release];
    }
}
