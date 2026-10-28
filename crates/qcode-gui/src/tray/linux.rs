//! Linux 托盘：ksni 的 StatusNotifierItem + dbusmenu。
//!
//! 注意：GPUI 的主线程运行在 tokio runtime 上下文里，ksni 的 blocking API
//! 内部会再 `block_on`，从主线程调用会 panic（"Cannot start a runtime from
//! within a runtime"）。所以托盘跑在自己的线程和 runtime 上，GPUI 只通过
//! 两个 `std::sync::mpsc` 通道收发：命令回传，快照下发。

use std::sync::mpsc::Sender;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{ToolTip, Tray, TrayMethods};

use super::state::{TrayCommand, TraySnapshot};

pub struct LinuxTray {
    state_tx: UnboundedSender<TraySnapshot>,
}

impl LinuxTray {
    pub fn update(&self, snapshot: TraySnapshot) {
        let _ = self.state_tx.send(snapshot);
    }
}

struct QcodeTray {
    snapshot: TraySnapshot,
    commands: Sender<TrayCommand>,
}

impl QcodeTray {
    fn send(&self, command: TrayCommand) {
        let _ = self.commands.send(command);
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

pub fn start(commands: Sender<TrayCommand>) -> Option<LinuxTray> {
    let (state_tx, mut state_rx) = tokio::sync::mpsc::unbounded_channel::<TraySnapshot>();
    let (ready_tx, ready_rx) = std::sync::mpsc::channel::<Result<(), String>>();

    std::thread::Builder::new()
        .name("qcode-tray".to_string())
        .spawn(move || {
            let runtime = match tokio::runtime::Builder::new_current_thread()
                .enable_all()
                .build()
            {
                Ok(runtime) => runtime,
                Err(error) => {
                    let _ = ready_tx.send(Err(error.to_string()));
                    return;
                }
            };

            runtime.block_on(async move {
                let tray = QcodeTray {
                    snapshot: TraySnapshot::default(),
                    commands,
                };
                let handle = match tray.spawn().await {
                    Ok(handle) => {
                        let _ = ready_tx.send(Ok(()));
                        handle
                    }
                    Err(error) => {
                        let _ = ready_tx.send(Err(error.to_string()));
                        return;
                    }
                };

                // 不能用阻塞接收：这个 runtime 同时驱动 DBus 服务，
                // 堵住线程会让 StatusNotifierItem 不再响应。
                while let Some(snapshot) = state_rx.recv().await {
                    if handle
                        .update(move |tray: &mut QcodeTray| tray.snapshot = snapshot)
                        .await
                        .is_none()
                    {
                        break;
                    }
                }
            });
        })
        .ok()?;

    match ready_rx.recv_timeout(Duration::from_secs(5)) {
        Ok(Ok(())) => Some(LinuxTray { state_tx }),
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
