use qcode_gui::gui_prefs::GuiPreferences;

#[test]
fn defaults_and_round_trip() {
    let dir = tempfile::tempdir().expect("temp");
    let defaults = GuiPreferences::load(dir.path());
    assert_eq!(defaults, GuiPreferences::default());

    let prefs = GuiPreferences {
        theme: "dark".to_string(),
        font_size: 16,
        code_font: "JetBrains Mono".to_string(),
        density: "compact".to_string(),
        send_mode: "cmd_enter".to_string(),
        recent_workspaces: vec!["/w/a".to_string(), "/w/b".to_string()],
        last_session: Some("s-1".to_string()),
        window_geometry: Some(qcode_gui::gui_prefs::WindowGeometry {
            x: 10.0,
            y: 20.0,
            width: 1200.0,
            height: 800.0,
        }),
    };
    prefs.save(dir.path()).expect("save");
    assert_eq!(GuiPreferences::load(dir.path()), prefs);
}

#[test]
fn recent_workspaces_are_deduplicated_and_capped() {
    let mut prefs = GuiPreferences::default();
    for index in 0..12 {
        prefs.record_workspace(std::path::Path::new(&format!("/w/{index}")));
    }
    prefs.record_workspace(std::path::Path::new("/w/5"));
    assert_eq!(prefs.recent_workspaces.len(), 10);
    assert_eq!(prefs.recent_workspaces[0], "/w/5");
    assert_eq!(
        prefs
            .recent_workspaces
            .iter()
            .filter(|p| *p == "/w/5")
            .count(),
        1
    );
}

#[test]
fn malformed_and_out_of_range_values_fall_back_safely() {
    let dir = tempfile::tempdir().expect("temp");
    std::fs::write(dir.path().join("gui.json"), "{not json").expect("write");
    assert_eq!(GuiPreferences::load(dir.path()), GuiPreferences::default());

    std::fs::write(
        dir.path().join("gui.json"),
        r#"{"theme":"neon","font_size":99,"code_font":"","density":"tiny"}"#,
    )
    .expect("write");
    let prefs = GuiPreferences::load(dir.path());
    assert_eq!(prefs.theme, "system");
    assert_eq!(prefs.font_size, 32);
    assert_eq!(prefs.code_font, "JetBrains Mono");
    assert_eq!(prefs.density, "comfortable");
}
