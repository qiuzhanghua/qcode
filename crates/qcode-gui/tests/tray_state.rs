//! 托盘状态的纯逻辑（ADR-0052）。qcode-gui 的 lib 测试被 `test = false` 关掉了，
//! 所以放在集成测试里跑。

use qcode_gui::tray::TraySnapshot;

/// 断言的是中文文案，而 `status_line()` 走 `qcode_i18n::tr()`。
///
/// 集成测试**不会**自动调用 `init()`，所以全局 locale 停在 rust-i18n 的默认值
/// `en` —— 不钉的话这三个测试直接红（它们此前一直是红的：`cargo test -p
/// qcode-gui --lib` 跑不到 `tests/`，而全量测试没人跑）。ADR-0105 §6 就是这条规矩。
fn pin() {
    qcode_i18n::pin_for_tests();
}

#[test]
fn idle_state() {
    pin();
    let s = TraySnapshot::default();
    assert_eq!(s.icon_name(), "utilities-terminal");
    assert_eq!(s.status_line(), "QCode · 空闲");
    assert!(!s.can_stop());
}

#[test]
fn running_state_enables_stop() {
    pin();
    let s = TraySnapshot::new(true, 0);
    assert_eq!(s.icon_name(), "media-playback-start");
    assert_eq!(s.status_line(), "QCode · 运行中");
    assert!(s.can_stop());
}

#[test]
fn approval_state_wins_over_running() {
    pin();
    let s = TraySnapshot::new(true, 2);
    assert_eq!(s.icon_name(), "dialog-warning");
    assert_eq!(s.status_line(), "QCode · 等待审批（2）");
    assert_eq!(s.tooltip(), s.status_line());
}
