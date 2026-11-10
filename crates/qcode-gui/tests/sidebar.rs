//! The sidebar's two pieces of state, and the invariants ADR-0063 puts on them.
//!
//! These are the parts of the collapse/accordion design that can be checked
//! without a window: which sections exist, that "both collapsed" is not
//! expressible, and that the choices survive a restart.

use qcode_gui::MainView;
use qcode_gui::SidebarSection;
use qcode_gui::gui_prefs::GuiPreferences;

#[test]
fn exactly_one_section_is_open_and_default_is_tasks() {
    // ADR-0063 §3: "both collapsed" is not a state. The type proves it — there
    // is no `None` variant to hold, and no `Option` wrapper to be `None`. This
    // test pins the consequence users feel: the default is 我的任务, not 定时任务.
    assert_eq!(SidebarSection::default(), SidebarSection::Tasks);
    assert_eq!(SidebarSection::ALL.len(), 2);
    assert_eq!(
        SidebarSection::ALL,
        [SidebarSection::Tasks, SidebarSection::Schedule]
    );
}

#[test]
fn a_section_round_trips_through_its_stored_spelling() {
    for section in SidebarSection::ALL {
        assert_eq!(SidebarSection::from_stored(section.as_str()), section);
    }
}

#[test]
fn an_unknown_stored_section_falls_back_to_tasks() {
    // A config written by a future version must not make the whole file
    // unreadable; ADR-0063 §3 says every value has a home, and the home is the
    // default section.
    assert_eq!(
        SidebarSection::from_stored("something-new"),
        SidebarSection::Tasks
    );
    assert_eq!(SidebarSection::from_stored(""), SidebarSection::Tasks);
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
fn a_fresh_install_starts_expanded_on_my_tasks() {
    let prefs = GuiPreferences::default();
    assert!(
        prefs.sidebar_expanded,
        "collapsed by default would hide the `+`"
    );
    assert_eq!(prefs.sidebar_section, "tasks");
}

#[test]
fn the_sidebar_state_survives_a_save_and_load() {
    // ADR-0063 §7: collapsing has a mouse entry now, so forgetting it on every
    // launch reads as broken. The whole point of persisting is that it comes back.
    let dir = std::env::temp_dir().join(format!("qcode-sidebar-prefs-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    let mut prefs = GuiPreferences::load(&dir);
    prefs.sidebar_expanded = false;
    prefs.sidebar_section = "schedule".to_string();
    prefs.save(&dir).expect("save");

    let loaded = GuiPreferences::load(&dir);
    assert!(!loaded.sidebar_expanded);
    assert_eq!(loaded.sidebar_section, "schedule");

    // And the two normalise back into the enum the window actually uses.
    assert_eq!(
        SidebarSection::from_stored(&loaded.sidebar_section),
        SidebarSection::Schedule
    );
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn a_corrupt_section_normalises_on_save_instead_of_loading_the_wrong_one() {
    let dir = std::env::temp_dir().join(format!("qcode-sidebar-corrupt-{}", std::process::id()));
    std::fs::create_dir_all(&dir).expect("temp dir");
    std::fs::write(
        GuiPreferences::path(&dir),
        r#"{"sidebar_expanded": false, "sidebar_section": "nope"}"#,
    )
    .expect("write");

    let loaded = GuiPreferences::load(&dir);
    // `load` normalises, so an unreadable section lands on the default rather
    // than making the rail point at a section that does not exist.
    assert_eq!(loaded.sidebar_section, "tasks");
    assert!(!loaded.sidebar_expanded, "the valid half must survive");
    let _ = std::fs::remove_dir_all(&dir);
}

#[test]
fn preferences_written_before_this_change_still_load() {
    // Everyone upgrading from v0.8.0 has a gui.json without these two keys.
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
    // Missing keys take their defaults: expanded, 我的任务.
    assert!(loaded.sidebar_expanded);
    assert_eq!(loaded.sidebar_section, "tasks");
    let _ = std::fs::remove_dir_all(&dir);
}
