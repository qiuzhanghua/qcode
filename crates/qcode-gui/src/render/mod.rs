//! Transcript rendering: turn cards, blocks, reasoning/tool rows, markdown.
//!
//! Pure view code, split out of `lib.rs` (P0 refactor); it reads `Shell` state
//! but never mutates anything beyond the entity notifications the views do.

#![allow(dead_code)]

use crate::*;
use crate::{
    ANSWER_LINE, ANSWER_SIZE, BODY_LINE, BODY_SIZE, Backend, CAPTION_SIZE, CODE_LINE, CODE_SIZE,
    DisplayUnit, MAIN_PAD_L, MAIN_PAD_R, PROMPT_INSET, RESULT_LINE, RESULT_SIZE, SECONDARY_LINE,
    SECONDARY_SIZE, Shell, TRANSCRIPT_PAD_L, TRANSCRIPT_PAD_R, TURN_RAIL_WIDTH, TaskRow,
    context_label, is_send_shortcut, sp, tool_status,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonCustomVariant, ButtonVariants as _};
use gpui_kit::component::diff::Diff;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::component::{Icon, Size, ThemeMode};
use gpui_kit::prelude::*;
use gpui_kit::*;
use qcode_core::event::{ToolDiff, ToolStatus};
use qcode_core::task::{RootAccess, RootState};
use qcode_ui_bridge::QueueOp;
use qcode_ui_state::{App, Block};

mod dialogs;
mod main;
pub(crate) mod rail;
mod schedule;
mod sidebar;
pub(crate) mod terminal_view;
mod transcript;

pub(crate) use sidebar::task_titles;
pub(crate) use transcript::*;

/// The brand blue: the `QC` mark as the dock and tray show it.
///
/// `#2F6BFF` is `build-icons.sh`'s `BRAND_BLUE` — the colour `windows/qcode-tray.ico`
/// and the Linux `IconPixmap` are drawn in, chosen there because Windows and Linux
/// do not tint tray icons and it stays visible on light *and* dark taskbars.
/// **This constant must match that script**: the whole point of using it in the
/// window is that the mark beside the window title is visibly the same mark as
/// the one in the dock, and two hard-coded blues that drift apart would make the
/// change cosmetic-only.
const BRAND_BLUE: u32 = 0x2F6BFF;

/// Dark mode gets one step of lightness, same hue (`#6691FF`, L 59 % → 70 %).
///
/// Not because the hue is wrong on a dark background: it is the *surface* that
/// differs. A taskbar sits behind the tray icon, so `#2F6BFF` is read against a
/// panel; the sidebar header is read against the theme background itself. The
/// numbers, against this theme's own colours (`default-theme.json`: light is
/// white, dark is `neutral-950` = `#0a0a0a`):
///
/// | | brand blue | brightened |
/// |---|---|---|
/// | light background | 4.50:1 | — |
/// | dark background | 4.40:1 | 6.66:1 |
///
/// 4.40 clears the 3:1 floor for a graphic but sits well under the 19:1 the
/// product name beside it gets, so the mark reads as washed out while the text
/// stays crisp. 6.66 lands next to the muted foreground the rest of the UI uses
/// (~7.9:1). Both themes therefore stay *recognisably* the tray blue; only the
/// light theme is the tray's own value — and even that arrives one 8-bit step
/// off (`#2E6AFF`) because it has to pass through `Hsla` to satisfy
/// `text_color`, and that round trip floors. One step on two channels is well
/// under a just-noticeable difference; `tests` pins it at ≤1 rather than
/// asserting a byte identity the colour type cannot deliver.
pub(crate) fn brand_blue(mode: ThemeMode) -> Hsla {
    match mode {
        ThemeMode::Light => rgb(BRAND_BLUE).into(),
        ThemeMode::Dark => rgb(0x6691FF).into(),
    }
}

impl Render for Shell {
    fn render(&mut self, window: &mut Window, cx: &mut Context<Self>) -> impl IntoElement {
        self.reload_on_config_change(cx);
        let bounds = window.bounds();
        let geometry = crate::gui_prefs::WindowGeometry {
            x: bounds.origin.x.as_f32(),
            y: bounds.origin.y.as_f32(),
            width: bounds.size.width.as_f32(),
            height: bounds.size.height.as_f32(),
        };
        if self.prefs.window_geometry.as_ref() != Some(&geometry) {
            self.prefs.window_geometry = Some(geometry.clone());
            // 几何是无主字段的“高频写入”，必须只写自己这一项：整份覆盖会把
            // 设置窗口/引擎刚写进 gui.json 的 `last_session` 冲回旧值。
            if let Err(error) = GuiPreferences::update(&self.data_dir, |prefs| {
                prefs.window_geometry = Some(geometry);
            }) {
                tracing::warn!(%error, "cannot save window geometry");
            }
        }
        div()
            .size_full()
            .relative()
            .flex()
            .flex_row()
            .key_context(if self.ui.approvals.current().is_some() {
                "Approval"
            } else {
                "Chat"
            })
            // A no-op listener, kept deliberately: it was a `QCODE_GUI_KEY_DEBUG`
            // scaffold whose body only printed, and whether the root consumes
            // mouse-down affects focus and event bubbling. Deleting the print is
            // safe; deleting the registration is a behaviour change to input
            // handling that nothing here can verify, so it is not this ADR's
            // business.
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {})
            .on_key_down({
                let entity = cx.entity();
                move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        entity.update(cx, |shell, cx| {
                            // The command-output panel owns Esc while it is open:
                            // it covers the transcript, and closing it has to put
                            // focus back in the composer (ADR-0087).
                            if shell.ui.command_output.is_some() {
                                shell.close_command_output(window, cx);
                                return;
                            }
                            let open = shell.rename_task.is_some()
                                || shell.pending_root_delete.is_some()
                                || shell.pending_task_delete.is_some()
                                || shell.task_menu.is_some()
                                || shell.schedule_menu_open
                                || shell.schedule_form.is_some()
                                || shell.pending_quit
                                || shell.pending_terminal_paste.is_some()
                                || shell.ui.diff_view.is_some();
                            if open {
                                shell.rename_task = None;
                                shell.pending_root_delete = None;
                                shell.pending_task_delete = None;
                                shell.task_menu = None;
                                shell.schedule_menu_open = false;
                                shell.schedule_form = None;
                                shell.pending_quit = false;
                                shell.pending_terminal_paste = None;
                                shell.close_diff_view(cx);
                                cx.notify();
                            }
                        });
                    } else if is_send_shortcut(&event.keystroke) {
                        entity.update(cx, |shell, cx| {
                            shell.send_prompt(window, cx);
                        });
                    } else if is_copy_shortcut(&event.keystroke) {
                        copy_selection(window, cx);
                    } else if is_select_all_shortcut(&event.keystroke) {
                        // Only when there is text to select: the transcript is
                        // the only selectable surface in this window, and the
                        // composer's own Cmd+A never reaches here.
                        gpui_kit::base::TextSelection::select_all(window, cx);
                    }
                }
            })
            .on_action(cx.listener(|this, _: &PasteImage, _window, cx| this.paste_clipboard(cx)))
            .on_action(
                cx.listener(|this, _: &ApproveOnce, _window, cx| this.answer(true, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &ApproveSession, _window, cx| this.answer(true, true, cx)),
            )
            .on_action(
                cx.listener(|this, _: &RejectApproval, _window, cx| this.answer(false, false, cx)),
            )
            .on_action(
                cx.listener(|this, _: &NewSessionShortcut, window, cx| {
                    this.new_session(window, cx)
                }),
            )
            .on_action(cx.listener(|this, _: &OpenTerminalShortcut, window, cx| {
                this.open_terminal(window, cx)
            }))
            .on_action(
                cx.listener(|this, _: &OpenSettingsShortcut, _window, cx| this.open_settings(cx)),
            )
            .on_action(cx.listener(|this, _: &CloseWindowShortcut, window, cx| {
                if this.minimize_on_close {
                    // 关闭 = 隐藏到托盘：窗口从任务栏消失，只留托盘图标
                    // （ADR-0052）。最小化还会把图标留在任务栏。
                    // 动作回调持有 `&mut App`，用延迟版本避开重入借用。
                    tray::hide_window_later(window, cx);
                } else {
                    window.remove_window();
                }
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &ToggleSidebarShortcut, _window, cx| {
                this.toggle_sidebar(cx);
            }))
            .on_action(cx.listener(|this, _: &ToggleThemeShortcut, _window, cx| {
                this.toggle_theme(cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomInShortcut, _window, cx| {
                this.zoom_font(1, cx);
            }))
            .on_action(cx.listener(|this, _: &ZoomOutShortcut, _window, cx| {
                this.zoom_font(-1, cx);
            }))
            .on_action(
                cx.listener(|this, _: &SendMessage, window, cx| this.send_prompt(window, cx)),
            )
            .on_action(cx.listener(|this, _: &StopTurnShortcut, _window, cx| {
                this.stop();
                cx.notify();
            }))
            .on_action(cx.listener(|this, _: &PrevTurnShortcut, _window, cx| {
                this.step_turn(-1, cx);
            }))
            .on_action(cx.listener(|this, _: &NextTurnShortcut, _window, cx| {
                this.step_turn(1, cx);
            }))
            // Sidebar and conversation share the window through a resizable
            // panel group, so the divider between them can be dragged and both
            // sides clamp to their minimum widths.
            .child({
                let main = match self.view {
                    crate::shell::MainView::Task => self.render_main(window, cx).into_any_element(),
                    crate::shell::MainView::Schedule => {
                        self.render_schedule_view(cx).into_any_element()
                    }
                    crate::shell::MainView::Terminal => {
                        self.render_terminal_view(cx).into_any_element()
                    }
                };
                if self.sidebar_expanded {
                    h_resizable("root-layout")
                        .child(
                            resizable_panel()
                                .size(px(240.0))
                                .size_range(px(SIDEBAR_MIN_WIDTH)..px(SIDEBAR_MAX_WIDTH))
                                .child(self.render_sidebar(cx).into_any_element()),
                        )
                        .child(
                            resizable_panel()
                                .size_range(px(MAIN_MIN_WIDTH)..Pixels::MAX)
                                .child(main),
                        )
                        .into_any_element()
                } else {
                    // The rail is a fixed width, not a resizable panel: there is
                    // nothing to drag at 48px, and a divider there would be a
                    // handle for a range with one value in it (ADR-0063 §1).
                    h_resizable("root-layout")
                        .child(
                            resizable_panel()
                                .size(px(SIDEBAR_RAIL_WIDTH))
                                .size_range(px(SIDEBAR_RAIL_WIDTH)..px(SIDEBAR_RAIL_WIDTH))
                                .child(self.render_sidebar_rail(cx).into_any_element()),
                        )
                        .child(
                            resizable_panel()
                                .size_range(px(MAIN_MIN_WIDTH)..Pixels::MAX)
                                .child(main),
                        )
                        .into_any_element()
                }
            })
            .child(self.render_delete_dialog(cx))
            .child(self.render_task_delete_dialog(cx))
            .child(self.render_terminal_paste_dialog(cx))
            .child(self.render_rename_dialog(cx))
            .child(self.render_schedule_form_dialog(cx))
            .child(self.render_quit_dialog(cx))
    }
}

/// `Cmd+C` over the transcript: whatever a drag selected, in plain text.
///
/// This is a key-down handler and not a [`KeyBinding`], and the reason is
/// precedence: GPUI ranks bindings by context depth first and by registration
/// order second, and a binding with no context predicate ties with every
/// predicate that matches at full depth. The app registers its keys after the
/// input components do, so a root `cmd-c` would outrank the composer's own
/// `Copy` and silently break copying inside a text field. A key-down listener
/// is only reached when no binding handled the keystroke, which is exactly the
/// case this shortcut is for.
///
/// Nothing happens without a selection: the alternative — clearing the
/// clipboard to "prove" the shortcut fired — destroys text the user copied a
/// moment ago from somewhere else.
fn copy_selection(window: &mut Window, cx: &mut gpui_kit::App) {
    if !gpui_kit::base::TextSelection::has_selection(window, cx) {
        return;
    }
    let text = gpui_kit::base::TextSelection::selected_text(window, cx);
    // The selection is the rendered text (`SelectionFormat::Plain`), so a drag
    // over a folded code block copies the twelve visible lines, not the whole
    // block. That is the price of "what you see is what you copy"; the code
    // block's own 复制 button is the way to get the full text.
    let text = text.trim();
    if text.is_empty() {
        return;
    }
    cx.write_to_clipboard(ClipboardItem::new_string(text.to_string()));
    cx.stop_propagation();
}

#[cfg(test)]
mod tests {
    // Narrow imports on purpose: `use super::*` here globs the crate root, and
    // that is enough extra name resolution to push this crate's `#[test]`
    // expansion past the recursion limit (same note as `window_icon.rs`).
    use super::BRAND_BLUE;
    use super::brand_blue;
    use gpui_kit::Rgba;
    use gpui_kit::component::ThemeMode;

    /// `BRAND_BLUE` is a second copy of a value that already exists in
    /// `build-icons.sh`, so it needs the same anchor `boot.rs` uses for the
    /// `.desktop` marker: assert the constant *is* the script's value.
    ///
    /// Why this matters: the point of tinting the sidebar mark is that it is
    /// visibly the same mark as the dock/tray one. If someone re-brands the icon
    /// set and only edits the script, the window keeps the old blue — and that
    /// failure is invisible in every test that only checks rendering, because in
    /// isolation both colours just look like "the brand colour".
    #[test]
    fn the_window_brand_blue_is_the_icon_sets_blue() {
        let script = include_str!(concat!(
            env!("CARGO_MANIFEST_DIR"),
            "/../qcode/res/icons/build-icons.sh"
        ));
        let expected = format!("BRAND_BLUE='#{BRAND_BLUE:06X}'");
        assert!(
            script.contains(&expected),
            "build-icons.sh must define {expected}; the window and the tray \
             would otherwise be two different blues"
        );
    }

    /// Light mode is the tray blue. Not byte-identical: `brand_blue` must hand
    /// back an `Hsla`, and `rgb(..) -> Hsla -> Rgba` floors, so `#2F6BFF` comes
    /// back as `#2E6AFF`, 1/255 low on red and green. That is what the colour
    /// type can promise, so this asserts a one-step tolerance instead of an
    /// equality that would fail on correct code.
    #[test]
    fn light_mode_is_the_brand_blue_to_within_one_step() {
        let rgba = Rgba::from(brand_blue(ThemeMode::Light));
        let packed = u32::from(rgba);
        // gpui packs as r<<24 | g<<16 | b<<8 | a.
        let (r, g, b) = (
            (packed >> 24) & 0xFF,
            (packed >> 16) & 0xFF,
            (packed >> 8) & 0xFF,
        );
        let (want_r, want_g, want_b) = (
            (BRAND_BLUE >> 16) & 0xFF,
            (BRAND_BLUE >> 8) & 0xFF,
            BRAND_BLUE & 0xFF,
        );
        assert!(
            r.abs_diff(want_r) <= 1 && g.abs_diff(want_g) <= 1 && b.abs_diff(want_b) <= 1,
            "light mode must stay within one 8-bit step of #{BRAND_BLUE:06X}, \
             got #{r:02X}{g:02X}{b:02X}"
        );
        assert_eq!(packed & 0xFF, 0xFF, "the mark must be fully opaque");
    }

    /// Dark mode brightens rather than switching hue: same hue, more lightness.
    /// If a future edit makes it a different blue outright, this catches it.
    #[test]
    fn dark_mode_brightens_the_same_hue() {
        let light = brand_blue(ThemeMode::Light);
        let dark = brand_blue(ThemeMode::Dark);
        assert!(
            (dark.h - light.h).abs() < 0.02,
            "dark mode must keep the hue: light {} vs dark {}",
            light.h,
            dark.h
        );
        assert!(
            dark.l > light.l,
            "dark mode must be lighter: light {} vs dark {}",
            light.l,
            dark.l
        );
    }
}
