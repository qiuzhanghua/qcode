//! 托盘状态的纯逻辑：不碰 DBus、不碰 GPUI，便于单测（ADR-0052）。

/// 菜单点击后要执行的动作。
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum TrayCommand {
    ShowWindow,
    NewTask,
    StopCurrent,
    Quit,
}

/// 托盘图标与菜单需要的最小状态。
#[derive(Clone, Debug, Default, PartialEq, Eq)]
pub struct TraySnapshot {
    /// 是否有 turn 正在运行。
    pub running: bool,
    /// 是否有待回答的审批（fail-closed，托盘只提示，不代答）。
    pub approvals: usize,
}

impl TraySnapshot {
    pub fn new(running: bool, approvals: usize) -> Self {
        Self { running, approvals }
    }

    /// freedesktop 图标名：待审批 > 运行中 > 空闲。
    pub fn icon_name(&self) -> &'static str {
        if self.approvals > 0 {
            "dialog-warning"
        } else if self.running {
            "media-playback-start"
        } else {
            "utilities-terminal"
        }
    }

    /// 菜单顶部那一行（disabled）。
    pub fn status_line(&self) -> String {
        if self.approvals > 0 {
            format!("QCode · 等待审批（{}）", self.approvals)
        } else if self.running {
            "QCode · 运行中".to_string()
        } else {
            "QCode · 空闲".to_string()
        }
    }

    pub fn tooltip(&self) -> String {
        self.status_line()
    }

    /// “停止当前任务”只在运行中可用。
    pub fn can_stop(&self) -> bool {
        self.running
    }
}
