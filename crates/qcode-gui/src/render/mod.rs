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
use gpui_kit::component::{Icon, Size};
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
                eprintln!("qcode-gui: cannot save window geometry: {error}");
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
            .on_mouse_down(MouseButton::Left, |_event, _window, _cx| {
                if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
                    eprintln!("qcode-gui root mouse down");
                }
            })
            .on_key_down({
                let entity = cx.entity();
                move |event, window, cx| {
                    if event.keystroke.key == "escape" {
                        entity.update(cx, |shell, cx| {
                            let open = shell.rename_task.is_some()
                                || shell.pending_root_delete.is_some()
                                || shell.pending_task_delete.is_some()
                                || shell.task_menu.is_some()
                                || shell.schedule_menu_open
                                || shell.schedule_form.is_some()
                                || shell.pending_quit
                                || shell.pending_terminal_paste.is_some();
                            if open {
                                shell.rename_task = None;
                                shell.pending_root_delete = None;
                                shell.pending_task_delete = None;
                                shell.task_menu = None;
                                shell.schedule_menu_open = false;
                                shell.schedule_form = None;
                                shell.pending_quit = false;
                                shell.pending_terminal_paste = None;
                                cx.notify();
                            }
                        });
                    } else if is_send_shortcut(&event.keystroke) {
                        entity.update(cx, |shell, cx| {
                            shell.send_prompt(window, cx);
                        });
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
