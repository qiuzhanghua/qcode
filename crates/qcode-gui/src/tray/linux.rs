//! Linux 托盘：ksni 的 StatusNotifierItem + dbusmenu。
//!
//! 注意：GPUI 的主线程运行在 tokio runtime 上下文里，ksni 的 blocking API
//! 内部会再 `block_on`，从主线程调用会 panic（"Cannot start a runtime from
//! within a runtime"）。所以托盘跑在自己的线程和 runtime 上，GPUI 只通过
//! 两个 `std::sync::mpsc` 通道收发：命令回传，快照下发。

use std::sync::OnceLock;
use std::sync::mpsc::Sender;
use std::time::Duration;
use tokio::sync::mpsc::UnboundedSender;

use ksni::menu::{MenuItem, StandardItem};
use ksni::{Icon, ToolTip, Tray, TrayMethods};

use super::state::{TrayCommand, TraySnapshot};

/// 面板常用的三个尺寸；内嵌进二进制，不依赖图标主题是否安装。
const ICONS: &[&[u8]] = &[
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../qcode/res/icons/linux/hicolor/22x22/apps/qcode.png"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../qcode/res/icons/linux/hicolor/32x32/apps/qcode.png"
    )),
    include_bytes!(concat!(
        env!("CARGO_MANIFEST_DIR"),
        "/../qcode/res/icons/linux/hicolor/48x48/apps/qcode.png"
    )),
];

static PIXMAPS: OnceLock<Vec<Icon>> = OnceLock::new();

/// 把内嵌 PNG 解成 SNI 要的 ARGB32（网络字节序）pixmap。
fn pixmaps() -> Vec<Icon> {
    PIXMAPS
        .get_or_init(|| {
            let mut pixmaps = Vec::new();
            for bytes in ICONS {
                let mut decoder = png::Decoder::new(std::io::Cursor::new(*bytes));
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
                let mut argb = Vec::with_capacity((info.width * info.height * 4) as usize);
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
                pixmaps.push(Icon {
                    width: info.width as i32,
                    height: info.height as i32,
                    data: argb,
                });
            }
            pixmaps
        })
        .clone()
}

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

    /// 空字符串：让 host 用 IconPixmap（我们自己的 QC 图标），而不是去
    /// 图标主题里找一个没安装的名字。三态由菜单状态行和 tooltip 表达。
    fn icon_name(&self) -> String {
        String::new()
    }

    fn icon_pixmap(&self) -> Vec<Icon> {
        pixmaps()
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
