//! Standalone spike binary: opens the window with an empty projection.
fn main() {
    qcode_gui::run(qcode_ui_state::App::new("deepseek", "deepseek-flash"));
}
