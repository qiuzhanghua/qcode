//! Transcript rendering: turn cards, blocks, reasoning/tool rows, markdown.
//!
//! Pure view code, split out of `lib.rs` (P0 refactor); it reads `Shell` state
//! but never mutates anything beyond the entity notifications the views do.

#![allow(dead_code)]

use crate::{
    ANSWER_LINE, ANSWER_SIZE, BODY_LINE, BODY_SIZE, Backend, CAPTION_SIZE, CODE_LINE, CODE_SIZE,
    DisplayUnit, MAIN_PAD_L, MAIN_PAD_R, PROMPT_INSET, RESULT_LINE, RESULT_SIZE,
    ROUND_HINT_THRESHOLD, SECONDARY_LINE, SECONDARY_SIZE, Shell, TRANSCRIPT_PAD_L,
    TRANSCRIPT_PAD_R, TURN_RAIL_WIDTH, TaskRow, WAITING_SPINNER, context_label, is_send_shortcut,
    sp, tool_status,
};
use gpui_kit::assets::IconName;
use gpui_kit::component::ActiveTheme as _;
use gpui_kit::component::Disableable as _;
use gpui_kit::component::Sizable as _;
use gpui_kit::component::button::{Button, ButtonVariants as _};
use gpui_kit::component::diff::Diff;
use gpui_kit::component::input::{Input, Textarea};
use gpui_kit::component::message_scroller::MessageScroller;
use gpui_kit::component::select::Select;
use gpui_kit::component::status_bar::StatusBar;
use gpui_kit::component::text::{TextView, TextViewStyle};
use gpui_kit::component::{Icon, Size};
use gpui_kit::prelude::*;
use gpui_kit::*;
use qcode_core::event::ToolStatus;
use qcode_core::task::{RootAccess, RootState};
use qcode_ui_bridge::QueueOp;
use qcode_ui_state::{App, Block};

mod dialogs;
mod main;
mod rail;
mod sidebar;
mod transcript;

pub(crate) use sidebar::task_titles;
pub(crate) use transcript::*;
