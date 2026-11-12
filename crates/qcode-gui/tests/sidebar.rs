//! The sidebar's two pieces of state, and the invariants ADR-0063 puts on them.
//!
//! These are the parts of the collapse/fold design that can be checked without
//! a window: which sections exist, that the two fold independently (both can be
//! open, both can be collapsed), and that the choices survive a restart.

use qcode_gui::MainView;
use qcode_gui::SidebarSection;
use qcode_gui::gui_prefs::GuiPreferences;

#[test]
fn the_two_sections_are_tasks_and_schedule_with_tasks_default() {
    // 我的任务 is the default section id; 定时任务 is the other one.
    assert_eq!(SidebarSection::default(), SidebarSection::Tasks);
    assert_ne!(SidebarSection::Tasks, SidebarSection::Schedule);
}

#[test]
fn the_schedule_board_is_its_own_main_view() {
    // ADR-0063 §8: 定时任务 is a third content view, not a section of the task
    // view. If this ever collapses back into `Task`, the rail's 定时任务 icon has
    // nowhere to go.
    assert_ne!(MainView::Schedule, MainView::Task);
    assert_ne!(MainView::Schedule, MainView::Terminal);
    assert_eq!(MainView::default(), MainView::Task);
}

#[test]
fn a_fresh_install_starts_with_both_sections_open() {
    let prefs = GuiPreferences::default();
    assert!(
        prefs.sidebar_expanded,
        "collapsed by default would hide the `+`"
    );
    // The two fold independently; a fresh install has both open (the old
    // accordion's "one section open" default would now hide 定时任务).
    assert!(prefs.tasks_section_open);
    assert!(prefs.schedule_section_open);
}

#[test]
fn the_sidebar_state_survives_a_save_and_load() {
    // Collapsing has a mouse entry now, so forgetting it on every launch reads
    // as broken. The whole point of persisting is that it comes back — including
    // an asymmetric fold (one open, one collapsed).
    let dir = std::env::temp_dir().join(format!("qcode-sidebar-prefs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut prefs = GuiPreferences::load(&dir);
    prefs.sidebar_expanded = false;
    prefs.tasks_section_open = false;
    prefs.schedule_section_open = true;
    prefs.save(&dir).expect("save");

    let loaded = GuiPreferences::load(&dir);
    assert!(!loaded.sidebar_expanded);
    assert!(!loaded.tasks_section_open, "a folded section stays folded");
    assert!(loaded.schedule_section_open);
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preferences_written_before_this_change_still_load() {
    // Everyone upgrading from v0.8.0 has a gui.json without these keys.
    let dir = std::env::temp_dir().join(format!("qcode-sidebar-old-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        GuiPreferences::path(&dir),
        r#"{"theme": "dark", "font_size": 16, "density": "compact"}"#,
    )
    .expect("write");

    let loaded = GuiPreferences::load(&dir);
    assert_eq!(loaded.theme, "dark");
    assert_eq!(loaded.font_size, 16);
    // Missing keys take their defaults: expanded, both sections open.
    assert!(loaded.sidebar_expanded);
    assert!(loaded.tasks_section_open);
    assert!(loaded.schedule_section_open);
    let _ = std::fs::remove_dir_all(&dir);
}
