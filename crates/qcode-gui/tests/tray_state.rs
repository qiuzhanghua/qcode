//! 托盘状态的纯逻辑（ADR-0052）。qcode-gui 的 lib 测试被 `test = false` 关掉了，
//! 所以放在集成测试里跑。

use qcode_gui::tray::TraySnapshot;

#[test]
fn idle_state() {
    let s = TraySnapshot::default();
    assert_eq!(s.icon_name(), "utilities-terminal");
    assert_eq!(s.status_line(), "QCode · 空闲");
    assert!(!s.can_stop());
}

#[test]
fn running_state_enables_stop() {
    let s = TraySnapshot::new(true, 0);
    assert_eq!(s.icon_name(), "media-playback-start");
    assert_eq!(s.status_line(), "QCode · 运行中");
    assert!(s.can_stop());
}

#[test]
fn approval_state_wins_over_running() {
    let s = TraySnapshot::new(true, 2);
    assert_eq!(s.icon_name(), "dialog-warning");
    assert_eq!(s.status_line(), "QCode · 等待审批（2）");
    assert_eq!(s.tooltip(), s.status_line());
}
