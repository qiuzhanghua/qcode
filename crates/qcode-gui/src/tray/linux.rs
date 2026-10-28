//! Linux 托盘：ksni 的 blocking 实现，菜单回调通过 std mpsc 送回 GPUI。

use std::sync::mpsc::Sender;

use ksni::blocking::{Handle, TrayMethods};
use ksni::menu::{MenuItem, StandardItem};
use ksni::{ToolTip, Tray};

use super::state::{TrayCommand, TraySnapshot};

pub struct LinuxTray {
    handle: Handle<QcodeTray>,
}

impl LinuxTray {
    pub fn update(&self, snapshot: TraySnapshot) {
        let _ = self.handle.update(move |tray: &mut QcodeTray| {
            tray.snapshot = snapshot;
        });
    }

    pub fn shutdown(&self) {
        drop(self.handle.shutdown());
    }
}

struct QcodeTray {
    snapshot: TraySnapshot,
    tx: Sender<TrayCommand>,
}

impl QcodeTray {
    fn send(&self, command: TrayCommand) {
        let _ = self.tx.send(command);
    }

    fn item(&self, label: &str, enabled: bool, command: TrayCommand) -> MenuItem<Self> {
        MenuItem::Standard(StandardItem {
            label: label.to_string(),
            enabled,
            activate: Box::new(move |tray: &mut QcodeTray| tray.send(command)),
            ..Default::default()
        })
    }
}

impl Tray for QcodeTray {
    fn id(&self) -> String {
        "qcode".to_string()
    }

    fn title(&self) -> String {
        "QCode".to_string()
    }

    fn icon_name(&self) -> String {
        self.snapshot.icon_name().to_string()
    }

    fn tool_tip(&self) -> ToolTip {
        ToolTip {
            icon_name: self.snapshot.icon_name().to_string(),
            icon_pixmap: Vec::new(),
            title: "QCode".to_string(),
            description: self.snapshot.tooltip(),
        }
    }

    fn activate(&mut self, _x: i32, _y: i32) {
        // 左键单击 = 显示主窗口（与菜单第一项一致）。
        self.send(TrayCommand::ShowWindow);
    }

    fn menu(&self) -> Vec<MenuItem<Self>> {
        vec![
            MenuItem::Standard(StandardItem {
                label: self.snapshot.status_line(),
                enabled: false,
                ..Default::default()
            }),
            MenuItem::Separator,
            self.item("显示主窗口", true, TrayCommand::ShowWindow),
            self.item("新建任务", true, TrayCommand::NewTask),
            self.item(
                "停止当前任务",
                self.snapshot.can_stop(),
                TrayCommand::StopCurrent,
            ),
            MenuItem::Separator,
            self.item("退出", true, TrayCommand::Quit),
        ]
    }
}

pub fn start(tx: Sender<TrayCommand>) -> Option<LinuxTray> {
    let tray = QcodeTray {
        snapshot: TraySnapshot::default(),
        tx,
    };
    match tray.spawn() {
        Ok(handle) => Some(LinuxTray { handle }),
        Err(error) => {
            // 没有 StatusNotifierWatcher 时不能拖垮主程序，退回关窗即退。
            eprintln!("qcode-gui: system tray unavailable: {error}");
            None
        }
    }
}
