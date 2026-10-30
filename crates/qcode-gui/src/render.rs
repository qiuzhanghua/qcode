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

pub(crate) fn display_units(app: &App) -> Vec<DisplayUnit> {
    let mut units = Vec::with_capacity(app.loose_blocks.len() + app.turns.len());
    for index in &app.loose_blocks {
        // Notices and standalone todo blocks are status-bar/tool-row content
        // only; a loose one would otherwise render as a stray row with no
        // turn around it.
        if matches!(
            app.blocks.get(*index),
            Some(Block::Notice(_)) | Some(Block::Todo(_))
        ) {
            continue;
        }
        units.push((*index, DisplayUnit::Loose(*index)));
    }
    for (turn_index, turn) in app.turns.iter().enumerate() {
        let start = turn
            .input_block
            .or_else(|| turn.process.first().copied())
            .or(turn.answer_block)
            .unwrap_or(usize::MAX);
        units.push((start, DisplayUnit::Turn(turn_index)));
    }
    units.sort_by_key(|(start, unit)| {
        (
            *start,
            match unit {
                DisplayUnit::Loose(_) => 0u8,
                DisplayUnit::Turn(_) => 1u8,
            },
        )
    });
    units.into_iter().map(|(_, unit)| unit).collect()
}
pub(crate) fn task_titles(
    tasks: &[TaskRow],
    backend: &dyn Backend,
) -> std::collections::BTreeMap<String, String> {
    tasks
        .iter()
        .filter_map(|row| {
            let title = row
                .task
                .title
                .clone()
                .or_else(|| backend.title(&row.task.id));
            title.map(|title| (row.task.id.clone(), title))
        })
        .collect()
}
pub(crate) fn unit_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    match shell.units.get(index).copied() {
        Some(DisplayUnit::Loose(block)) => block_row(block, shell, entity),
        Some(DisplayUnit::Turn(turn)) => turn_row(turn, shell, entity),
        None => {
            // The trailing scroller row holds the blinking typing indicator
            // while a turn streams; once it finishes the row is empty.
            if shell.app.status.running {
                typing_row(shell.waiting_frame as u32)
            } else {
                div().into_any_element()
            }
        }
    }
}
pub(crate) fn conversation_icon(name: IconName) -> Icon {
    Icon::new(name)
        .with_size(Size::Small)
        .text_color(rgb(0x888888))
}
/// A blinking "typing…" row pinned under the transcript while a turn runs.
/// `frame` advances on a timer so the indicator visibly flashes.
pub(crate) fn typing_row(frame: u32) -> AnyElement {
    // Two frames on, two frames off: a slower blink than the status spinner.
    let opacity = if frame % 4 < 2 { 1.0 } else { 0.35 };
    div()
        .flex()
        .flex_row()
        .items_center()
        .px_6()
        .py_1()
        .text_size(sp(SECONDARY_SIZE))
        .text_color(rgb(0x888888))
        .child(div().opacity(opacity).child("执行中…"))
        .into_any_element()
}
pub(crate) fn format_duration(duration: time::Duration) -> String {
    let seconds = duration.whole_seconds().max(0) as u64;
    if seconds < 60 {
        format!("{seconds}s")
    } else {
        format!("{}m{}s", seconds / 60, seconds % 60)
    }
}
pub(crate) fn reasoning_preview(text: &str) -> String {
    let line = text
        .lines()
        .find(|line| !line.trim().is_empty())
        .unwrap_or("")
        .trim();
    short_preview(line, 60)
}
pub(crate) fn live_reasoning_row(shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    // This row only renders while thinking is streaming: always show the
    // live text instead of a preview.
    let expanded = true;
    let preview = reasoning_preview(&shell.app.reasoning_streaming);
    let mut row = div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .px_6()
        .py_1()
        .items_start();
    row = row.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .w_full()
            .id("reasoning-live-row")
            .cursor_pointer()
            .text_size(sp(SECONDARY_SIZE))
            .text_color(rgb(0x888888))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| {
                        shell.toggle_reasoning(usize::MAX, cx);
                    });
                }
            })
            .child(conversation_icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(conversation_icon(IconName::Brain))
            .child("思考")
            .children((!expanded && !preview.is_empty()).then(|| {
                div()
                    .text_color(rgb(0x777777))
                    .child(format!("· {preview}"))
            })),
    );
    if expanded {
        row = row.child(
            div().w_full().pl_8().pr_6().pb_2().child(
                div()
                    .w_full()
                    .rounded(px(6.0))
                    .bg(rgba(0x7f7f7f08))
                    .border_1()
                    .border_color(rgba(0x7f7f7f20))
                    .p_2()
                    .text_size(sp(SECONDARY_SIZE))
                    .line_height(sp(SECONDARY_LINE))
                    .text_color(rgb(0x777777))
                    .child(shell.app.reasoning_streaming.clone()),
            ),
        );
    }
    row.into_any_element()
}
pub(crate) fn reasoning_row(
    block: usize,
    entry: &qcode_ui_state::ReasoningEntry,
    shell: &Shell,
    entity: &Entity<Shell>,
) -> AnyElement {
    // While this block is the live streaming thinking, force it expanded so
    // the text visibly appends; once the thinking completes the block is
    // auto-folded (the shell clears the reasoning expansion) but stays
    // expandable through the header toggle.
    let streaming = shell.app.status.running && shell.app.streaming_reasoning_block == Some(block);
    let expanded = streaming || shell.reasoning_expanded(block);
    let preview = reasoning_preview(&entry.text);
    let mut row = div()
        .flex()
        .flex_col()
        .gap_1()
        .w_full()
        .px_6()
        .py_1()
        .items_start();
    row = row.child(
        div()
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .w_full()
            .id(format!("reasoning-row-{block}"))
            .cursor_pointer()
            .text_size(sp(SECONDARY_SIZE))
            .text_color(rgb(0x888888))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| shell.toggle_reasoning(block, cx));
                }
            })
            .child(conversation_icon(if expanded {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }))
            .child(conversation_icon(IconName::Brain))
            .child("思考")
            .children((!expanded && !preview.is_empty()).then(|| {
                div()
                    .text_color(rgb(0x777777))
                    // 折叠预览只占剩余宽度，超长时以省略号截断，不撑出对话框。
                    .flex_1()
                    .truncate()
                    .child(format!("· {preview}"))
            })),
    );
    if expanded {
        row = row.child(
            div().w_full().pl_8().pr_6().pb_2().child(
                div()
                    .w_full()
                    .rounded(px(6.0))
                    .bg(rgba(0x7f7f7f08))
                    .border_1()
                    .border_color(rgba(0x7f7f7f20))
                    .p_2()
                    .child(
                        TextView::markdown(format!("reasoning-body-{block}"), entry.text.clone())
                            .code_block_actions(|_, _, _| div().into_any_element())
                            .text_size(sp(SECONDARY_SIZE))
                            .line_height(sp(SECONDARY_LINE))
                            .text_color(rgb(0x777777)),
                    ),
            ),
        );
    }
    row.into_any_element()
}
pub(crate) fn tool_label(name: &str) -> String {
    match name {
        "run_shell" | "shell" => "运行命令",
        "list_dir" => "列出目录",
        "glob" => "查找文件",
        "grep" | "search" => "搜索",
        "read_file" | "read" => "读取文件",
        "write_file" | "write" => "写入文件",
        "edit_file" | "edit" | "apply_patch" => "修改文件",
        "todo_write" | "todo" => "更新待办",
        "subagent" | "task" => "子代理",
        _ => name,
    }
    .to_string()
}
pub(crate) fn tool_icon_name(name: &str) -> IconName {
    match name {
        "run_shell" | "shell" => IconName::SquareTerminal,
        "list_dir" | "glob" => IconName::FolderSearch,
        "grep" | "search" => IconName::Search,
        "read_file" | "read" => IconName::FileText,
        "write_file" | "edit_file" | "apply_patch" => IconName::FilePen,
        "todo_write" | "todo" => IconName::ListChecks,
        "subagent" | "task" => IconName::Bot,
        _ => IconName::Wrench,
    }
}
pub(crate) fn short_preview(text: &str, limit: usize) -> String {
    let mut value: String = text
        .split_whitespace()
        .collect::<Vec<_>>()
        .join(" ")
        .chars()
        .take(limit)
        .collect();
    if text.chars().count() > limit {
        value.push('…');
    }
    value
}
/// Heuristic: the output reads as a markdown list when every non-empty line
/// starts with a bullet (`- `, `* `, `+ `) or a number (`1. `).
pub(crate) fn looks_like_list(text: &str) -> bool {
    let lines: Vec<&str> = text
        .lines()
        .filter(|line| !line.trim().is_empty())
        .collect();
    !lines.is_empty()
        && lines.iter().all(|line| {
            let trimmed = line.trim_start();
            if trimmed.starts_with("- ") || trimmed.starts_with("* ") || trimmed.starts_with("+ ") {
                return true;
            }
            let bytes = trimmed.as_bytes();
            let digits = bytes
                .iter()
                .take_while(|byte| byte.is_ascii_digit())
                .count();
            digits > 0
                && bytes.get(digits) == Some(&b'.')
                && bytes.get(digits + 1).is_none_or(|byte| *byte == b' ')
        })
}
pub(crate) fn turn_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    let turn = &shell.app.turns[index];
    let mut card = div().flex().flex_col().gap_1().w_full();

    if let Some(block) = turn.input_block {
        card = card.child(block_row(block, shell, entity));
    }

    // `Block::Notice` 是状态栏内容，不再在对话流里渲染成一行 “note › …”。
    // 执行过程的项数也只统计真正会渲染的块。
    let process_blocks: Vec<usize> = turn
        .process
        .iter()
        .copied()
        .filter(|block| !matches!(shell.app.blocks.get(*block), Some(Block::Notice(_))))
        .collect();
    let has_process =
        !process_blocks.is_empty() || (turn.running() && !shell.app.reasoning_streaming.is_empty());
    let show_process = !shell.collapse_all;

    if has_process && show_process {
        // One container for the whole execution process (thinking + tools).
        // It opens with the turn and the header toggles it; once the turn
        // finishes it auto-folds and the answer renders below, outside the
        // container.
        let process_open = shell.process_open_turns.contains(&index);
        let mut process = div().flex().flex_col().gap_0().w_full().items_start();
        for block in &process_blocks {
            if let Block::Reasoning(entry) = &shell.app.blocks[*block] {
                if entry.text.is_empty()
                    && turn.running()
                    && !shell.app.reasoning_streaming.is_empty()
                {
                    process = process.child(live_reasoning_row(shell, entity));
                } else {
                    process = process.child(reasoning_row(*block, entry, shell, entity));
                }
            } else {
                process = process.child(block_row(*block, shell, entity));
            }
        }
        // Build the running-time label *at render time* so it ticks in sync
        // with the existing 200ms waiting_ticker (which already calls
        // cx.notify() while a turn is live). Once the turn ends the duration
        // is sealed from Turn::ended_at and stays fixed.
        let status_label = match turn.status {
            Some(qcode_core::event::TurnStatus::Completed) => "done",
            Some(qcode_core::event::TurnStatus::Failed) => "failed",
            Some(qcode_core::event::TurnStatus::Cancelled) => "cancelled",
            None if turn.running() => "running",
            None => "done",
        };
        let elapsed_label = if turn.running() {
            let now = time::OffsetDateTime::now_utc();
            let live = (now - turn.started_at).max(time::Duration::ZERO);
            format!("耗时{}", format_duration(live))
        } else {
            turn.duration()
                .map(|d| format!("耗时{}", format_duration(d)))
                .unwrap_or_default()
        };
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .justify_start()
            .gap_2()
            .px_6()
            .py_1()
            .id(format!("process-toggle-{index}"))
            .cursor_pointer()
            .text_size(sp(SECONDARY_SIZE))
            .text_color(rgb(0x888888))
            .on_click({
                let entity = entity.clone();
                move |_, _, cx| {
                    entity.update(cx, |shell, cx| shell.toggle_process(index, cx));
                }
            })
            .child(format!("执行过程 · {} 项", process_blocks.len()))
            .child(status_label)
            .child(elapsed_label)
            .child(conversation_icon(if process_open {
                IconName::ChevronDown
            } else {
                IconName::ChevronRight
            }));
        // The divider only shows once the turn has finished and its final
        // answer block renders. A streaming preview must not split the
        // process from its result early: the turn may still be executing
        // tools while the first words arrive.
        let has_answer = turn.answer_block.is_some();
        card = card
            .child(header)
            .children(process_open.then_some(process))
            .children(has_answer.then(|| {
                div()
                    .mx_6()
                    .my_1()
                    .h(px(1.0))
                    .bg(rgba(0x7f7f7f20))
                    .into_any_element()
            }));
    }

    if let Some(block) = turn.answer_block {
        card = card.child(block_row(block, shell, entity));
    } else if turn.running() && !shell.app.streaming.is_empty() {
        card = card.child(
            div()
                .px_6()
                .py_2()
                .text_size(sp(ANSWER_SIZE))
                .line_height(sp(ANSWER_LINE))
                .child(
                    TextView::markdown(
                        format!("streaming-turn-{index}"),
                        shell.app.streaming.clone(),
                    )
                    .code_block_actions(|_, _, _| div().into_any_element())
                    .text_size(sp(ANSWER_SIZE))
                    .line_height(sp(ANSWER_LINE)),
                ),
        );
    }
    card.into_any_element()
}
pub(crate) fn markdown_segments(text: &str) -> Vec<MarkdownSegment> {
    let mut segments = Vec::new();
    let mut text_buf = String::new();
    let mut code_buf = String::new();
    let mut language = String::new();
    let mut in_code = false;
    for line in text.split_inclusive('\n') {
        let trimmed = line.trim_start();
        if trimmed.starts_with("```") {
            if in_code {
                segments.push(MarkdownSegment::Code {
                    language: language.clone(),
                    body: code_buf.trim_end_matches('\n').to_string(),
                });
                code_buf.clear();
                in_code = false;
            } else {
                if !text_buf.is_empty() {
                    segments.push(MarkdownSegment::Text(std::mem::take(&mut text_buf)));
                }
                language = trimmed.trim_start_matches('`').trim().to_string();
                in_code = true;
            }
            continue;
        }
        if in_code {
            code_buf.push_str(line);
        } else {
            text_buf.push_str(line);
        }
    }
    if in_code {
        segments.push(MarkdownSegment::Code {
            language,
            body: code_buf.trim_end_matches('\n').to_string(),
        });
    } else if !text_buf.is_empty() {
        segments.push(MarkdownSegment::Text(text_buf));
    }
    segments
}
/// with the box above it, whose bottom corners are squared off in that case.
pub(crate) fn fold_toggle(key: String, expanded: bool, entity: &Entity<Shell>) -> AnyElement {
    div()
        .w_full()
        .flex()
        .flex_row()
        .items_center()
        .justify_center()
        .py_1()
        .rounded_t(px(0.0))
        .rounded_b(px(6.0))
        .bg(rgba(0x7f7f7f08))
        .border_1()
        .border_color(rgba(0x7f7f7f20))
        .cursor_pointer()
        .id(format!("fold-toggle-{key}"))
        .text_color(rgb(0x888888))
        .hover(|style| style.bg(rgba(0x7f7f7f14)))
        .on_click({
            let key = key.clone();
            let entity = entity.clone();
            move |_, _, cx| {
                entity.update(cx, |shell, cx| shell.toggle_code(key.clone(), cx));
            }
        })
        .child(conversation_icon(if expanded {
            IconName::ArrowUp
        } else {
            IconName::ArrowDown
        }))
        .into_any_element()
}
pub(crate) fn markdown_with_folds(
    index: usize,
    text: &str,
    shell: &Shell,
    entity: &Entity<Shell>,
) -> AnyElement {
    let mut column = div().flex().flex_col().gap_1().px_6().py_2();
    for (segment_index, segment) in markdown_segments(text).into_iter().enumerate() {
        match segment {
            MarkdownSegment::Text(body) => {
                column = column.child(
                    TextView::markdown(format!("assistant-{index}-{segment_index}"), body)
                        .text_size(sp(ANSWER_SIZE))
                        .line_height(sp(ANSWER_LINE)),
                );
            }
            MarkdownSegment::Code { language, body } => {
                let line_count = body.lines().count();
                let key = format!("{index}:{segment_index}");
                let expanded = shell.expanded_code.contains(&key);
                let folded = line_count > 20;
                let visible = if expanded || !folded {
                    body.clone()
                } else {
                    body.lines().take(12).collect::<Vec<_>>().join("\n")
                };
                let fence = format!("```{language}\n{visible}\n```");
                // One single code block — no extra wrapping box or header,
                // indented one level from the surrounding answer text. With a
                // fold toggle the code block loses its bottom corners so the
                // toggle below reads as one piece with it.
                let mut block = div().flex().flex_col().gap_0().my_1().pl_4();
                let mut code = TextView::markdown(format!("assistant-code-{key}"), fence)
                    .code_block_actions(|_, _, _| div().into_any_element())
                    .text_size(sp(CAPTION_SIZE))
                    .line_height(sp(CODE_LINE));
                if folded {
                    code = code.style(
                        TextViewStyle::default()
                            .code_block(StyleRefinement::default().rounded_b(px(0.0))),
                    );
                }
                block = block.child(code);
                if folded {
                    block = block.child(fold_toggle(key, expanded, entity));
                }
                column = column.child(block);
            }
        }
    }
    column.into_any_element()
}
pub(crate) fn block_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    if index >= shell.app.blocks.len() {
        return div()
            .px_4()
            .py_2()
            .child(
                TextView::markdown(format!("streaming-{index}"), shell.app.streaming.clone())
                    .code_block_actions(|_, _, _| div().into_any_element()),
            )
            .into_any_element();
    }

    match &shell.app.blocks[index] {
        Block::User(text) => div()
            .flex()
            .flex_col()
            .items_end()
            .w_full()
            .px_6()
            .py_2()
            .child(
                div()
                    .max_w(px(560.0))
                    .bg(rgba(0x7f7f7f18))
                    .rounded(px(12.0))
                    .px_4()
                    .py_2()
                    .text_size(sp(BODY_SIZE))
                    .line_height(sp(BODY_LINE))
                    .child(
                        TextView::markdown(format!("user-{index}"), text.clone())
                            .text_size(sp(BODY_SIZE))
                            .line_height(sp(BODY_LINE)),
                    ),
            )
            .into_any_element(),
        Block::Assistant(text) => markdown_with_folds(index, text, shell, entity),
        Block::Tool(tool) => {
            let label = tool_label(&tool.name);
            let summary = short_preview(&tool.summary, 150);
            let expanded = shell.tool_expanded(&tool.call_id);
            let status = (tool.status != Some(ToolStatus::Ok)).then(|| tool_status(tool.status));
            let mut row = div().flex().flex_col().w_full().px_6().py_1().items_start();
            let mut header = div()
                .flex()
                .flex_row()
                .items_center()
                .justify_start()
                .w_full()
                .gap_2()
                .id(format!("tool-row-{index}"))
                .cursor_pointer()
                .text_size(sp(SECONDARY_SIZE))
                .text_color(rgb(0x888888))
                .on_click({
                    let call_id = tool.call_id.clone();
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |shell, cx| {
                            shell.toggle_tool(&call_id, cx);
                        });
                    }
                })
                .child(conversation_icon(if expanded {
                    IconName::ChevronDown
                } else {
                    IconName::ChevronRight
                }))
                .child(conversation_icon(tool_icon_name(&tool.name)))
                .child(label)
                .children((!summary.is_empty()).then(|| {
                    div()
                        .text_color(rgb(0x777777))
                        .child(format!("· {summary}"))
                }));
            if let Some(status) = status {
                header = header.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(rgb(0x999999))
                        .child(status),
                );
            }
            row = row.child(header);
            if expanded {
                let mut details = div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .w_full()
                    .pl_8()
                    .pr_6()
                    .pb_2();
                let command = tool
                    .arguments
                    .get("command")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                let output_line_count = tool.output.lines().count();
                let foldable = output_line_count > 20;
                let output_key = format!("{index}:output");
                let output_expanded = shell.expanded_code.contains(&output_key);
                let output_visible = if output_expanded || !foldable {
                    tool.output.clone()
                } else {
                    tool.output.lines().take(12).collect::<Vec<_>>().join("\n")
                };
                if command.is_some() || !tool.output.is_empty() {
                    // The command and its result share one box, with a blank
                    // line between them. When the output is folded the box
                    // loses its bottom corners so the toggle below reads as
                    // one piece with it.
                    let mut content = div()
                        .w_full()
                        .rounded_t(px(6.0))
                        .rounded_b(if foldable { px(0.0) } else { px(6.0) })
                        .bg(rgba(0x7f7f7f08))
                        .border_1()
                        .border_color(rgba(0x7f7f7f20))
                        .p_2()
                        .flex()
                        .flex_col();
                    if let Some(command) = &command {
                        content = content.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_baseline()
                                .gap_1()
                                .text_size(sp(RESULT_SIZE))
                                .line_height(sp(RESULT_LINE))
                                .child(
                                    div()
                                        .text_size(sp(RESULT_SIZE))
                                        .line_height(sp(RESULT_LINE))
                                        .text_color(rgb(0xc8a86a))
                                        .child("$"),
                                )
                                .child(
                                    // Markdown inline code (backticks) renders
                                    // at 0.875x the TextView size, so bump the
                                    // base size up so the command lands on the
                                    // same size as the plain output below.
                                    TextView::markdown(
                                        format!("tool-command-{index}"),
                                        format!("`{command}`"),
                                    )
                                    .text_size(sp(RESULT_SIZE / 0.875))
                                    .line_height(sp(RESULT_LINE)),
                                ),
                        );
                    }
                    if !tool.output.is_empty() {
                        if command.is_some() {
                            content = content.child(div().h(px(8.0)));
                        }
                        // Lists render as markdown lists; anything else stays
                        // plain light text — one box, no nested code block.
                        if looks_like_list(&output_visible) {
                            content = content.child(
                                div()
                                    .text_size(sp(RESULT_SIZE))
                                    .line_height(sp(RESULT_LINE))
                                    .child(
                                        TextView::markdown(
                                            format!("tool-output-{index}"),
                                            output_visible,
                                        )
                                        .text_size(sp(RESULT_SIZE))
                                        .line_height(sp(RESULT_LINE)),
                                    ),
                            );
                        } else {
                            // Plain multi-line output: markdown hard breaks
                            // (two trailing spaces) keep every line break, so
                            // the text renders exactly like the original one
                            // box — no nested code-block frame, same size and
                            // colour — while staying selectable.
                            let selectable = output_visible.replace('\n', "  \n");
                            content = content.child(
                                div()
                                    .text_size(sp(RESULT_SIZE))
                                    .line_height(sp(RESULT_LINE))
                                    .child(
                                        TextView::markdown(
                                            format!("tool-output-plain-{index}"),
                                            selectable,
                                        )
                                        .text_size(sp(RESULT_SIZE))
                                        .line_height(sp(RESULT_LINE))
                                        .text_color(rgb(0x888888)),
                                    ),
                            );
                        }
                    }
                    // The fold toggle joins the box as one piece when the
                    // output is long (squared corners, no gap).
                    if foldable {
                        details = details.child(
                            div()
                                .flex()
                                .flex_col()
                                .gap_0()
                                .w_full()
                                .child(content)
                                .child(fold_toggle(output_key, output_expanded, entity)),
                        );
                    } else {
                        details = details.child(content);
                    }
                }
                row = row.child(details);
            }
            row.into_any_element()
        }
        Block::Subagent { text, .. } => div()
            .px_6()
            .py_1()
            .text_size(sp(CODE_SIZE))
            .text_color(rgb(0x888888))
            .child(format!("child › {text}"))
            .into_any_element(),
        Block::Reasoning(entry) => div()
            .px_6()
            .py_1()
            .text_size(sp(SECONDARY_SIZE))
            .line_height(sp(SECONDARY_LINE))
            .text_color(rgb(0x999999))
            .child(
                TextView::markdown(format!("reasoning-{index}"), entry.text.clone())
                    .code_block_actions(|_, _, _| div().into_any_element())
                    .text_size(sp(SECONDARY_SIZE))
                    .line_height(sp(SECONDARY_LINE)),
            )
            .into_any_element(),
        // The todo_write tool already renders its full content inside the tool
        // row; the separate Todo block would show the same items again in a
        // stray gray box, so it is hidden (loose ones are filtered in
        // display_units too).
        Block::Todo(_) => div().into_any_element(),
        Block::Notice(_) => div().into_any_element(),
        Block::Skill(text) => div()
            .px_6()
            .py_2()
            .text_size(sp(SECONDARY_SIZE))
            .line_height(sp(SECONDARY_LINE))
            .child(
                TextView::markdown(format!("skill-{index}"), text.clone())
                    .text_size(sp(SECONDARY_SIZE))
                    .line_height(sp(SECONDARY_LINE)),
            )
            .into_any_element(),
    }
}
pub(crate) enum MarkdownSegment {
    Text(String),
    Code { language: String, body: String },
}

impl Shell {
    /// 退出确认：只在任务运行中或等待审批时出现。
    pub(crate) fn render_quit_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        if !self.pending_quit {
            return div().into_any_element();
        }
        let theme = cx.theme();
        div()
            .absolute()
            .inset_0()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child("退出 QCode？")
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child("当前任务仍在运行，退出会中断它。"),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(
                                Button::new("cancel-quit").label("取消").on_click(
                                    cx.listener(|this, _, _window, cx| this.cancel_quit(cx)),
                                ),
                            )
                            .child(Button::new("confirm-quit").label("退出").danger().on_click(
                                cx.listener(|this, _, _window, cx| this.confirm_quit(cx)),
                            )),
                    ),
            )
            .into_any_element()
    }

    /// The pending queue dock above the composer: each queued message shows
    /// its preview plus steer / move / remove actions, mirroring dsh's queue
    /// dock. Steer is only offered while a turn is running.
    pub(crate) fn render_queue_dock(&self, cx: &mut Context<Self>) -> AnyElement {
        let theme = cx.theme();
        let border = theme.border;
        let running = self.turn_in_flight;
        let mut rows = Vec::with_capacity(self.steering.len() + self.queue.len());
        // Steering already handed to the engine but not claimed yet: keep it
        // visible as pending so the user can take it back.
        for item in &self.steering {
            let id = item.id.clone();
            let preview: String = item.text.replace('\n', " ").chars().take(48).collect();
            rows.push(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .gap_2()
                    .px_2()
                    .py_1()
                    .rounded(px(6.0))
                    .bg(rgba(0x2f6bff14))
                    .child(
                        div()
                            .flex_1()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(preview),
                    )
                    .child(
                        div()
                            .text_size(sp(CAPTION_SIZE))
                            .text_color(rgb(0x5b7fbf))
                            .child("等待下一步"),
                    )
                    .child(
                        Button::new(format!("steer-undo-{id}"))
                            .label("撤回")
                            .on_click(cx.listener(move |this, _, _, _| {
                                this.backend.queue_op(id.clone(), QueueOp::Unsteer);
                            })),
                    )
                    .into_any_element(),
            );
        }
        for item in &self.queue {
            let id = item.id.clone();
            let preview: String = item.text.replace('\n', " ").chars().take(48).collect();
            let mut row = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .px_2()
                .py_1()
                .rounded(px(6.0))
                .bg(rgba(0x7f7f7f14))
                .child(
                    div()
                        .flex_1()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(theme.muted_foreground)
                        .child(preview),
                );
            if running {
                let steer_id = id.clone();
                row = row.child(
                    Button::new(format!("queue-steer-{steer_id}"))
                        .label("推送")
                        .on_click(cx.listener(move |this, _, _, _| {
                            this.backend.queue_op(steer_id.clone(), QueueOp::Steer);
                        })),
                );
            }
            let up_id = id.clone();
            row = row.child(
                Button::new(format!("queue-up-{up_id}"))
                    .label("↑")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(up_id.clone(), QueueOp::MoveUp);
                    })),
            );
            let down_id = id.clone();
            row = row.child(
                Button::new(format!("queue-down-{down_id}"))
                    .label("↓")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(down_id.clone(), QueueOp::MoveDown);
                    })),
            );
            let remove_id = id.clone();
            row = row.child(
                Button::new(format!("queue-remove-{remove_id}"))
                    .label("×")
                    .on_click(cx.listener(move |this, _, _, _| {
                        this.backend.queue_op(remove_id.clone(), QueueOp::Remove);
                    })),
            );
            rows.push(row.into_any_element());
        }
        div()
            .flex()
            .flex_col()
            .gap_1()
            .w_full()
            .rounded(px(10.0))
            .border_1()
            .border_color(border)
            .bg(rgba(0x7f7f7f0a))
            .px_2()
            .py_1()
            .children(rows)
            .into_any_element()
    }

    pub(crate) fn render_floating_actions(&self, cx: &mut Context<Self>) -> AnyElement {
        if self.app.turns.is_empty() {
            return div().into_any_element();
        }
        div()
            .absolute()
            .bottom(px(20.0))
            .right(px(28.0))
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .p_1()
            .rounded(px(10.0))
            .bg(rgba(0x7f7f7f20))
            .border_1()
            .border_color(rgba(0x7f7f7f20))
            .child(
                Button::new("jump-latest")
                    .icon(IconName::ArrowDown)
                    .accessibility_label("跳转到底部")
                    .tooltip("跳转到底部")
                    .on_click(cx.listener(|this, _, _window, cx| this.jump_to_bottom(cx))),
            )
            .into_any_element()
    }

    pub(crate) fn render_turn_rail(
        &mut self,
        window: &Window,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        // Expire any pending rail-popup close timer whose time has come.
        if let Some(at) = self.rail_popup_close_at
            && std::time::Instant::now() >= at
        {
            self.rail_popup = None;
            self.rail_popup_close_at = None;
        }
        if self.app.turns.len() < 2 || window.bounds().size.width.as_f32() < 900.0 {
            return div().into_any_element();
        }
        let total = self.app.turns.len();
        let active = self.active_turn_index();
        // At most 15 dots are shown; with more turns the window scrolls so the
        // active dot stays centred (clamped at the list edges).
        const MAX_DOTS: usize = 15;
        let start = if total <= MAX_DOTS {
            0
        } else {
            let center = active.unwrap_or(total - 1).min(total - 1);
            (center.saturating_sub(MAX_DOTS / 2)).min(total - MAX_DOTS)
        };
        let end = (start + MAX_DOTS).min(total);
        let mut rail = div()
            .w(px(TURN_RAIL_WIDTH))
            .flex()
            .flex_col()
            .items_center()
            .justify_center()
            .gap_2();
        for index in start..end {
            // Colour fades with distance from the active turn: the current one
            // is darkest, one step away slightly lighter, farther ones lightest.
            let dot_color = match active
                .map(|active| index.abs_diff(active))
                .unwrap_or(usize::MAX)
            {
                0 => rgb(0x7a7a7a),
                1 => rgb(0xbcbcbc),
                _ => rgb(0xd8d8d8),
            };
            let show_popover = self.rail_popup == Some(index);
            let dot = div()
                .w(px(8.0))
                .h(px(8.0))
                .rounded(px(4.0))
                .bg(dot_color)
                .hover(|style| style.bg(rgb(0xa8a8a8)))
                .cursor_pointer()
                .id(format!("turn-rail-trigger-{index}"))
                .on_hover(cx.listener(move |this, hovered, _window, cx| {
                    if *hovered {
                        this.rail_popup = Some(index);
                        this.rail_popup_close_at = None;
                    } else if this.rail_popup == Some(index) {
                        // Short grace window only to let the pointer travel
                        // from the dot into the popover. Once the pointer
                        // leaves both, the popover must hide — the popover's
                        // own outer div on_hover(false) handles that path,
                        // and this timer catches the case where the user
                        // jumps straight from dot → nowhere.
                        this.rail_popup_close_at =
                            Some(std::time::Instant::now() + std::time::Duration::from_millis(200));
                    }
                    cx.notify();
                }))
                .on_click(cx.listener(move |this, _, _window, cx| {
                    this.jump_to_turn(index, cx);
                }));
            let popup =
                gpui_kit::base::Popup::new(format!("rail-popup-{index}"), dot.into_any_element())
                    // RightCenter: the popover's right edge snaps to the dot's
                    // left centre, so the card appears on the *left* side of
                    // the rail (over the transcript).
                    .anchor(gpui_kit::Anchor::RightCenter)
                    .offset(px(6.0));
            let popup = if show_popover {
                popup.content(self.build_rail_popover(cx, index))
            } else {
                popup
            };
            rail = rail.child(popup);
        }
        rail.into_any_element()
    }

    /// Build the hover-popover body for a rail dot: the turn's question and
    /// answer stacked in two labelled sections. Long answers are truncated
    /// with an ellipsis. Clicking anywhere in the card jumps to that turn.
    pub(crate) fn build_rail_popover(
        &mut self,
        cx: &mut Context<Self>,
        turn_index: usize,
    ) -> gpui_kit::AnyElement {
        const QUESTION_LIMIT: usize = 120;
        const ANSWER_LIMIT: usize = 260;
        let (question, answer) = match self.app.turns.get(turn_index) {
            Some(turn) => {
                let question = turn
                    .input_block
                    .and_then(|idx| self.app.blocks.get(idx))
                    .and_then(|block| match block {
                        Block::User(text) => Some(text.trim()),
                        _ => None,
                    })
                    .map(|text| short_preview(text, QUESTION_LIMIT))
                    .unwrap_or_else(|| format!("Turn {}", turn.number));
                let answer = turn
                    .answer_block
                    .and_then(|idx| self.app.blocks.get(idx))
                    .and_then(|block| match block {
                        Block::Assistant(text) => Some(text.trim()),
                        _ => None,
                    })
                    .map(|text| short_preview(text, ANSWER_LIMIT))
                    .unwrap_or_else(|| {
                        if turn.running() {
                            "（回答中…）".to_string()
                        } else {
                            "（暂无回答）".to_string()
                        }
                    });
                (question, answer)
            }
            None => (String::new(), String::new()),
        };
        let question_color = rgb(0x222222);
        let answer_color = rgb(0x777777);
        div()
            .id(format!("rail-popover-{turn_index}"))
            .w(px(300.0))
            .flex()
            .flex_col()
            .bg(rgb(0xffffff))
            .rounded(px(6.0))
            .border_1()
            .border_color(rgb(0xe3e3e3))
            .cursor_pointer()
            .on_hover(cx.listener(move |this, hovered, _window, cx| {
                if *hovered {
                    // Pointer inside the popover — cancel any pending close
                    // so the dot's 200ms grace window doesn't shut us down.
                    this.rail_popup_close_at = None;
                } else {
                    // Pointer fully left the popover. Schedule a zero-delay
                    // close (expires next render) so the card snaps hidden.
                    this.rail_popup_close_at = Some(std::time::Instant::now());
                }
                cx.notify();
            }))
            .on_click(cx.listener(move |this, _, _window, cx| {
                this.rail_popup = None;
                this.rail_popup_close_at = None;
                this.jump_to_turn(turn_index, cx);
            }))
            // 问题：字号大、颜色深；回答：字号小、颜色浅，靠这两点区分，
            // 不再需要标题或分隔线。
            .child(
                div()
                    .px_2()
                    .pt_2()
                    .pb_1()
                    .text_size(sp(BODY_SIZE))
                    .text_color(question_color)
                    .child(question),
            )
            .child(
                div()
                    .px_2()
                    .pt_1()
                    .pb_2()
                    .text_size(sp(12.0))
                    .text_color(answer_color)
                    .child(answer),
            )
            .into_any_element()
    }

    pub(crate) fn render_task_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((task_id, title)) = self.pending_task_delete.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(440.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(format!("删除任务 “{title}”？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(
                                "将删除本地任务记录和对话历史，不可恢复。不会删除目录中的文件。",
                            ),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-delete-task").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_task_delete = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-delete-task")
                                    .label("删除")
                                    .danger()
                                    .on_click(cx.listener(move |this, _, window, cx| {
                                        this.pending_task_delete = None;
                                        this.delete_task_now(&task_id, window, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_rename_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(state) = &self.rename_task else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        let input = state.input.clone();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child("重命名任务")
                    .child(Input::new(&input).w_full())
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-rename-task").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.rename_task = None;
                                    cx.notify();
                                }),
                            ))
                            .child(Button::new("confirm-rename-task").label("保存").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.save_rename_task(cx);
                                }),
                            )),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_delete_dialog(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some((task_id, label, path)) = self.pending_root_delete.clone() else {
            return div().into_any_element();
        };
        let theme = cx.theme();
        div()
            .absolute()
            .top_0()
            .left_0()
            .size_full()
            .flex()
            .items_center()
            .justify_center()
            .bg(rgba(0x00000055))
            .child(
                div()
                    .w(px(420.0))
                    .flex()
                    .flex_col()
                    .gap_3()
                    .rounded(px(10.0))
                    .bg(theme.popover)
                    .border_1()
                    .border_color(theme.border)
                    .p_4()
                    .text_color(theme.popover_foreground)
                    .child(format!("移除目录 @{label}？"))
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(theme.muted_foreground)
                            .child(format!("不会删除 {path} 中的文件，只会从当前任务中移除。")),
                    )
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .justify_end()
                            .gap_2()
                            .child(Button::new("cancel-remove-root").label("取消").on_click(
                                cx.listener(|this, _, _window, cx| {
                                    this.pending_root_delete = None;
                                    cx.notify();
                                }),
                            ))
                            .child(
                                Button::new("confirm-remove-root")
                                    .label("移除")
                                    .danger()
                                    .on_click(cx.listener(move |this, _, _window, cx| {
                                        this.pending_root_delete = None;
                                        this.remove_task_root_now(&task_id, &label, cx);
                                    })),
                            ),
                    ),
            )
            .into_any_element()
    }

    pub(crate) fn render_task_menu(
        &self,
        task_id: &str,
        title: &str,
        cx: &mut Context<Self>,
    ) -> AnyElement {
        let theme = cx.theme();
        let running = self.active.as_deref() == Some(task_id) && self.turn_in_flight;
        div()
            .w(px(160.0))
            .flex()
            .flex_col()
            .gap_1()
            .rounded(px(8.0))
            .bg(theme.popover)
            .border_1()
            .border_color(theme.border)
            .p_1()
            .text_color(theme.popover_foreground)
            .child(
                Button::new(format!("task-rename-{task_id}"))
                    .label("重命名")
                    .tooltip(if running {
                        "运行中改名将在 Turn 结束后生效"
                    } else {
                        "重命名任务"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        move |this, _, window, cx| {
                            cx.stop_propagation();
                            this.open_rename_task(&task_id, window, cx);
                        }
                    })),
            )
            .child(
                Button::new(format!("task-delete-{task_id}"))
                    .label("删除任务")
                    .danger()
                    .disabled(running)
                    .tooltip(if running {
                        "结束当前 Turn 后可删除"
                    } else {
                        "删除任务"
                    })
                    .on_click(cx.listener({
                        let task_id = task_id.to_string();
                        let title = title.to_string();
                        move |this, _, _window, cx| {
                            cx.stop_propagation();
                            this.request_delete_task(&task_id, title.clone(), cx);
                        }
                    })),
            )
            .into_any_element()
    }

    pub(crate) fn render_task_roots(&self, cx: &mut Context<Self>) -> AnyElement {
        let Some(row) = self.active_task_row() else {
            return div().into_any_element();
        };
        let task_id = row.task.id.clone();
        let roots = self.task_roots_for_edit(&task_id);
        let pending = self
            .pending_roots
            .as_ref()
            .is_some_and(|(id, _)| id == &task_id);
        let mut section = div().flex().flex_col().gap_1().pl_4().pt_1();
        for root in roots {
            let label = root.label.clone();
            let path = root.path.display().to_string();
            let missing = root.state() == RootState::Missing;
            let access = match root.access {
                RootAccess::ReadOnly => "RO",
                RootAccess::ReadWrite => "RW",
            };
            let primary = if root.primary { "★" } else { "☆" };
            let mut line = div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .h(px(24.0))
                .child(
                    Button::new(format!("root-alias-{task_id}-{label}"))
                        .text()
                        .compact()
                        .label(format!("@{label}"))
                        .tooltip(path.clone()),
                );
            if missing {
                line = line.child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(rgb(0xd06060))
                        .child("丢失"),
                );
            }
            let line = line
                .child(
                    Button::new(format!("root-access-{task_id}-{label}"))
                        .compact()
                        .label(access)
                        .tooltip("切换只读 / 读写")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.toggle_task_root_access(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-primary-{task_id}-{label}"))
                        .compact()
                        .label(primary)
                        .tooltip("设为主目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.set_task_primary_root(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-relink-{task_id}-{label}"))
                        .compact()
                        .label("↗")
                        .tooltip("重新关联目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.relink_task_root(&task_id, &label, cx);
                            })
                        }),
                )
                .child(
                    Button::new(format!("root-remove-{task_id}-{label}"))
                        .compact()
                        .label("×")
                        .tooltip("移除目录")
                        .on_click({
                            let task_id = task_id.clone();
                            let label = label.clone();
                            cx.listener(move |this, _, _window, cx| {
                                this.request_remove_task_root(&task_id, &label, path.clone(), cx);
                            })
                        }),
                );
            section = section.child(line);
        }
        if pending {
            section = section.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("当前 Turn 结束后应用"),
            );
        } else if self.turn_in_flight {
            section = section.child(
                div()
                    .text_size(sp(CAPTION_SIZE))
                    .text_color(rgb(0x888888))
                    .child("Turn 结束后应用"),
            );
        }
        section.into_any_element()
    }

    pub(crate) fn render_sidebar(&self, cx: &mut Context<Self>) -> impl IntoElement {
        let row_hover = cx.theme().sidebar_accent.opacity(0.55);
        let row_active = cx.theme().sidebar_accent;
        let mut rows: Vec<AnyElement> = Vec::new();
        for row in &self.tasks {
            let id = row.task.id.clone();
            let active = self.active.as_deref() == Some(row.task.id.as_str());
            let marker = if active { "●" } else { "○" };
            let full_title = self
                .titles
                .get(&row.task.id)
                .cloned()
                .unwrap_or_else(|| "新任务".to_string());
            let title = full_title.clone();
            // A task with a vanished directory is broken until it is relinked,
            // so say so in the row itself instead of hiding it behind the
            // expanded root editor.
            let missing = row
                .task
                .roots
                .iter()
                .filter(|root| root.state() == RootState::Missing)
                .count();
            let wonky = if missing > 0 {
                " · 目录丢失".to_string()
            } else {
                String::new()
            };
            let label = format!("{marker} {title}{wonky}");
            let menu_open = self.task_menu.as_deref() == Some(id.as_str());
            let show_actions = self.hovered_task.as_deref() == Some(id.as_str()) || menu_open;
            let hover_id = id.clone();
            let mut task_row = div()
                .relative()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .w_full()
                .h(px(30.0))
                .px_2()
                .rounded(px(6.0))
                .cursor_pointer()
                .when(active, |this| {
                    this.bg(row_active).font_weight(FontWeight::SEMIBOLD)
                })
                .when(!active, |this| this.hover(|style| style.bg(row_hover)))
                .when(!rows.is_empty(), |this| this.mt_2())
                .id(format!("task-row-{}", row.task.id))
                .on_hover(cx.listener(move |this, hovered: &bool, _window, cx| {
                    if *hovered {
                        this.hovered_task = Some(hover_id.clone());
                        cx.notify();
                    }
                }))
                .on_click(cx.listener(move |this, _, window, cx| {
                    this.task_menu = None;
                    this.resume(id.clone(), window, cx)
                }))
                .child(
                    div()
                        .flex_1()
                        .min_w(px(0.0))
                        .overflow_hidden()
                        .text_size(sp(BODY_SIZE))
                        .whitespace_nowrap()
                        .child(label),
                );
            let controls = div()
                .id(format!("task-actions-{}", row.task.id))
                .occlude()
                .absolute()
                .right_1()
                .top_0()
                .h(px(30.0))
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .invisible()
                .when(show_actions, |this| this.visible())
                .child(
                    Button::new(format!("task-menu-{}", row.task.id))
                        .compact()
                        .label("…")
                        .tooltip("任务操作")
                        .on_click(cx.listener({
                            let task_id = row.task.id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.task_menu =
                                    if this.task_menu.as_deref() == Some(task_id.as_str()) {
                                        None
                                    } else {
                                        Some(task_id.clone())
                                    };
                                cx.notify();
                            }
                        })),
                )
                .child(
                    Button::new(format!("task-add-dir-{}", row.task.id))
                        .compact()
                        .label("+")
                        .tooltip("添加目录")
                        .on_click(cx.listener({
                            let task_id = row.task.id.clone();
                            move |this, _, _window, cx| {
                                cx.stop_propagation();
                                this.task_menu = None;
                                this.add_task_root(&task_id, cx);
                            }
                        })),
                );
            task_row = task_row.child(controls);
            if menu_open {
                task_row = task_row.child(
                    deferred(
                        anchored()
                            .anchor(Anchor::TopRight)
                            .offset(point(px(0.0), px(30.0)))
                            .snap_to_window_with_margin(px(8.0))
                            .child(self.render_task_menu(&row.task.id, &full_title, cx)),
                    )
                    .with_priority(gpui_kit::base::POPUP_PRIORITY),
                );
            }
            rows.push(task_row.into_any_element());
            if active {
                rows.push(self.render_task_roots(cx));
            }
        }

        div()
            .w_full()
            .h_full()
            .flex()
            .flex_col()
            .gap_1()
            .p_2()
            .id("task-sidebar")
            // Sidebar sits on the base `sidebar` token, one step lighter than
            // the row hover (`sidebar_accent` at reduced opacity), so the list
            // reads as a surface and the hovered row still stands out.
            .bg(cx.theme().sidebar)
            .overflow_y_scroll()
            .child(
                Button::new("open-settings")
                    .label("Settings")
                    .on_click(cx.listener(|this, _, _window, cx| this.open_settings(cx))),
            )
            .child(
                div()
                    .flex()
                    .flex_row()
                    .items_center()
                    .justify_between()
                    .child("我的任务")
                    .child(
                        Button::new("new-task")
                            .label("+")
                            .tooltip("新建任务")
                            .on_click(cx.listener(|this, _, window, cx| {
                                this.new_session(window, cx);
                            })),
                    ),
            )
            .children(rows)
    }

    pub(crate) fn render_main(
        &mut self,
        window: &mut Window,
        cx: &mut Context<Self>,
    ) -> impl IntoElement {
        self.poll(cx);
        // 执行中且输入框有内容时，发送按钮应保持可用（发送进排队队列）；
        // 只有执行中且输入框为空时，按钮才变成停止红点。
        let composer_has_draft = !self.input.read(cx).value().trim().is_empty()
            || !self.attachments.is_empty()
            || !self.file_refs.is_empty();
        if self.app.status.running && !self.waiting_ticker {
            self.waiting_ticker = true;
            self.tick_waiting(cx);
        }
        if self.pending_send {
            self.pending_send = false;
            self.send_prompt(window, cx);
        }
        let entity = cx.entity();
        let theme = cx.theme();
        let popover = theme.popover;
        let border = theme.border;
        let list = MessageScroller::new("transcript", self.scroller.clone(), {
            let entity = entity.clone();
            move |index, _window, cx| {
                let shell = entity.read(cx);
                unit_row(index, shell, &entity)
            }
        })
        .scrollbar(false)
        .jump_button(false)
        // The scroller applies its own top/bottom padding to the virtual list.
        // That padding is included in the list's internal scroll extent but
        // not in `max_offset_for_scrollbar`, so the wheel clamp (`content_size
        // - viewport`) stops a few pixels short of the real end and never
        // re-engages tail follow. Keep the list itself unpadded and hold the
        // breathing room on the surrounding container instead.
        .with_list_style(StyleRefinement::default().py(px(0.0)));

        // Header label: the session title when the task has one (first user
        // message), else 「新任务」——一个还没有第一条消息的任务就是新任务，
        // 不展示不可识别的原始 session id。
        let header_label = self
            .active
            .as_deref()
            .and_then(|id| self.titles.get(id))
            .cloned()
            .unwrap_or_else(|| "新任务".to_string());

        // Keep the picker's selection in sync with the kernel's model
        // (a resume or a settings rebuild may change it).
        let current_model = self.app.status.model.clone();
        let needs_sync = {
            let selected = self.model_select.read(cx).selected_value().cloned();
            selected.as_ref() != Some(&current_model)
        };
        if needs_sync {
            self.model_select.update(cx, |state, cx| {
                state.set_selected_value(&current_model, window, cx);
            });
        }
        let header = div()
            .flex()
            .flex_row()
            .items_center()
            .gap_2()
            .px_2()
            .py_4()
            .child(format!("{header_label} · {} turn(s)", self.app.turns.len()))
            .child(div().flex_grow(1.0))
            .child(div().w(px(220.0)).child(
                // Default `Length::Auto` sizes the dropdown to the trigger
                // width, so the popup matches the box above it.
                Select::new(&self.model_select).placeholder("model"),
            ));

        // 本次任务执行时间：只在新 turn 运行中显示，数值与「执行过程」头部的
        // 实时耗时一致（靠 200ms ticker 刷新）；turn 中断或结束、执行过程耗时
        // 消失时，这行蓝字也一并隐藏。
        let task_elapsed = self.app.turns.last().and_then(|turn| {
            turn.running().then(|| {
                let now = time::OffsetDateTime::now_utc();
                let live = (now - turn.started_at).max(time::Duration::ZERO);
                format!("本次任务耗时 {}", format_duration(live))
            })
        });

        div()
            .flex()
            .flex_col()
            .flex_grow(1.0)
            .min_w_0()
            .pl(px(MAIN_PAD_L))
            .pr(px(MAIN_PAD_R))
            .child(header)
            .child({
                let rail = self.render_turn_rail(window, cx);
                div()
                    .relative()
                    .flex()
                    .flex_row()
                    .flex_grow(1.0)
                    .min_h_0()
                    .child(
                        div()
                            .id("transcript-scroll")
                            .flex()
                            .flex_col()
                            .flex_grow(1.0)
                            .min_h_0()
                            .pl(px(TRANSCRIPT_PAD_L))
                            .pr(px(TRANSCRIPT_PAD_R))
                            .pt(px(8.0))
                            .pb(px(32.0))
                            // GPUI keeps a text selection live until the mouse
                            // button-up arrives, and a wheel scroll then
                            // drags/extends it along the pointer — so a
                            // phantom button state (or a lost mouse-up) makes
                            // mere scrolling select text. Cancel the selection
                            // on scroll, the way browsers do.
                            .on_scroll(|_, window, cx| {
                                gpui_kit::base::TextSelection::clear(window, cx);
                            })
                            .child(list),
                    )
                    .child(rail)
                    .child(self.render_floating_actions(cx))
            })
            .children(self.diff.as_ref().map(|state| {
                let height = if self.diff_expanded {
                    px(480.0)
                } else {
                    px(220.0)
                };
                div()
                    .flex()
                    .flex_col()
                    .h(height)
                    .child(
                        div()
                            .flex()
                            .flex_row()
                            .gap_1()
                            .child(
                                Button::new("toggle-diff")
                                    .label(if self.diff_expanded {
                                        "Collapse diff"
                                    } else {
                                        "Expand diff"
                                    })
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.diff_expanded = !this.diff_expanded;
                                        cx.notify();
                                    })),
                            )
                            .child(
                                Button::new("close-diff")
                                    .label("Close")
                                    .on_click(cx.listener(|this, _, _window, cx| {
                                        this.diff = None;
                                        cx.notify();
                                    })),
                            ),
                    )
                    .child(Diff::new(state))
            }))
            // 执行时间：一行无背景的蓝色字，放在所有提示框（权限、报错、
            // 50 轮）上方、输入框之上；无提示框时则直接位于输入框上方。
            .children(task_elapsed.map(|label| {
                div()
                    .w_full()
                    .pl(px(MAIN_PAD_L + 10.0))
                    .pr(px(MAIN_PAD_R))
                    .pb_1()
                    .child(
                        div()
                            .text_size(sp(SECONDARY_SIZE))
                            .text_color(rgb(0x3b82f6))
                            .child(label),
                    )
            }))
            .child(
                div()
                    .flex()
                    .flex_col()
                    .gap_1()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .children(
                        (self.app.turns.len() >= ROUND_HINT_THRESHOLD
                            && self.round_hint_dismissed_for != self.active)
                            .then(|| {
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_2()
                                    .w_full()
                                    // 只圆上两角，下沿直角。
                                    .rounded_tl(px(8.0))
                                    .rounded_tr(px(8.0))
                                    .bg(rgba(0x7f7f7f0d))
                                    .px_2()
                                    .py_1()
                                    .child(
                                        div()
                                            .flex_grow(1.0)
                                            .text_size(sp(SECONDARY_SIZE))
                                            .child(
                                                "当前任务已达到 50 轮。创建新任务可能会获得更好的结果。",
                                            ),
                                    )
                                    .child(
                                        Button::new("round-hint-new-task")
                                            .compact()
                                            .label("新建任务")
                                            .on_click(cx.listener(|this, _, window, cx| {
                                                this.new_session(window, cx);
                                            })),
                                    )
                                    .child(
                                        div()
                                            .id("round-hint-close")
                                            .cursor_pointer()
                                            .text_color(rgb(0x000000))
                                            .text_size(sp(SECONDARY_SIZE))
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.round_hint_dismissed_for =
                                                    this.active.clone();
                                                cx.notify();
                                            }))
                                            .child("×"),
                                    )
                            })
                            // 和权限框 / 报错框同样的内缩量，三者宽度一致。
                            .map(|hint| {
                                div()
                                    .w_full()
                                    .pl(px(PROMPT_INSET))
                                    .pr(px(PROMPT_INSET))
                                    .child(hint)
                            }),
                    )
                    .children(
                        // 权限问询：有问询时以提示卡片占位展示（输入框隐藏），
                        // 避免输入框自身的边框/圆角从提示框背后透出来。
                        self.approvals.current().map(|view| {
                            let kind: String = match view.tool.as_str() {
                                "run_shell" | "shell" => "执行命令".to_string(),
                                "read_file" | "write_file" | "edit_file" | "apply_patch"
                                | "list_dir" | "glob" | "read" | "write" | "edit"
                                | "search" => "读写文件".to_string(),
                                _ => tool_label(&view.tool),
                            };
                            // 拦截原因映射成中文，其余情况直接展示原文。
                            let reason_cn = match view.reason.as_str() {
                                reason if reason.contains("process execution") => "进程执行策略",
                                reason if reason.contains("write outside workspace") => {
                                    "工作区外写入策略"
                                }
                                reason if reason.contains("multi-file write") => {
                                    "多文件写入策略"
                                }
                                reason if reason.contains("external tool") => "外部工具策略",
                                reason if reason.contains("network access") => "网络访问策略",
                                reason if reason.contains("matched rule") => "权限规则",
                                reason => reason,
                            };
                            let dir = if self.cwd.is_empty() {
                                "当前目录".to_string()
                            } else {
                                self.cwd.clone()
                            };
                            // 第一部分：说明文案（黑字）；第二部分：具体的执行
                            // 内容（灰字），字体都比此前略大。
                            let explain = format!(
                                "是否允许本次操作使用{kind}权限：执行{summary}时，被{reason_cn}拦截，无法在{dir}目录上执行写入，需要提升操作权限；此次操作不会改动任何文件的内容和所有者。",
                                kind = kind,
                                summary = view.summary,
                                reason_cn = reason_cn,
                                dir = dir,
                            );
                            div()
                                .w_full()
                                .rounded_t(px(10.0))
                                .border_1()
                                .border_color(border)
                                .bg(rgb(0xffffff))
                                .flex()
                                .flex_col()
                                // 整卡一圈边框、上两角圆角；title 是黄色色块，
                                // 内容区无独立边框（自然没有上边框）、四角直角。
                                .child(
                                    div()
                                        .rounded_tl(px(10.0))
                                        .rounded_tr(px(10.0))
                                        .bg(rgb(0xfacc15))
                                        .px_3()
                                        .py_1()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_size(sp(16.0))
                                                .line_height(sp(26.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x000000))
                                                .child("等待审批"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_3()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x000000))
                                                .child(explain),
                                        )
                                        .child(
                                            div()
                                                .pl(px(20.0))
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x666666))
                                                .child(format!("> {}", view.summary)),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .child(
                                                    Button::new("allow").label("允许")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(true, false, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new("allow-session")
                                                        .label("总是允许")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(true, true, cx);
                                                            },
                                                        )),
                                                )
                                                .child(
                                                    Button::new("deny").label("拒绝")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.answer(false, false, cx);
                                                            },
                                                        )),
                                                ),
                                        ),
                                )
                        }),
                    )
                    .children(
                        // 报错提示：同款卡片，红色 title，单「关闭」按钮。
                        self.error_banner.clone().map(|message| {
                            div()
                                .w_full()
                                .rounded_t(px(10.0))
                                .border_1()
                                .border_color(border)
                                .bg(rgb(0xffffff))
                                .flex()
                                .flex_col()
                                .child(
                                    div()
                                        .rounded_tl(px(10.0))
                                        .rounded_tr(px(10.0))
                                        .bg(rgb(0xe8463a))
                                        .px_3()
                                        .py_1()
                                        .flex()
                                        .flex_row()
                                        .items_center()
                                        .child(
                                            div()
                                                .text_size(sp(16.0))
                                                .line_height(sp(26.0))
                                                .font_weight(FontWeight::SEMIBOLD)
                                                .text_color(rgb(0x000000))
                                                .child("请求报错 · 完整信息"),
                                        ),
                                )
                                .child(
                                    div()
                                        .px_3()
                                        .pb_2()
                                        .flex()
                                        .flex_col()
                                        .gap_2()
                                        .child(
                                            div()
                                                .text_size(sp(15.0))
                                                .line_height(sp(24.0))
                                                .text_color(rgb(0x000000))
                                                .child(message),
                                        )
                                        .child(
                                            div()
                                                .flex()
                                                .flex_row()
                                                .gap_2()
                                                .child(
                                                    Button::new("dismiss-error")
                                                        .label("关闭")
                                                        .on_click(cx.listener(
                                                            |this, _, _window, cx| {
                                                                this.error_banner = None;
                                                                cx.notify();
                                                            },
                                                        )),
                                                ),
                                        ),
                                )
                        }),
                    )
                    .children(self.restoring.as_ref().map(|_| {
                        div()
                            .w_full()
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(border)
                            .bg(rgb(0xffffff))
                            .px_3()
                            .py_2()
                            .text_size(sp(SECONDARY_SIZE))
                            .line_height(sp(SECONDARY_LINE))
                            .text_color(rgb(0x777777))
                            .child("已提交，等待引擎确认…")
                    }))
                    .when(
                        self.approvals.current().is_none()
                            && self.error_banner.is_none()
                            && self.restoring.is_none(),
                        |this| {
                            this.child(
                                div()
                                    .relative()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .w_full()
                                    .rounded(px(10.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .px_3()
                            .py_2()
                            .child({
                let entity = cx.entity();
                let mut input_box = div()
                    .flex()
                    .flex_col()
                    .gap_1();
                // Pending follow-up queue shown above the input; each row can
                // steer into the running turn, move, or drop.
                if !self.queue.is_empty() || !self.steering.is_empty() {
                    input_box = input_box.child(self.render_queue_dock(cx));
                }
                // Pending file references shown as removable chips above the
                // input, aligned to its left edge; wraps once too wide.
                if !self.file_refs.is_empty() {
                    let mut chips = div()
                        .flex()
                        .flex_row()
                        .flex_wrap()
                        .gap_1();
                    for path in &self.file_refs {
                        let name = path
                            .file_name()
                            .and_then(|name| name.to_str())
                            .unwrap_or("<file>")
                            .to_string();
                        let remove_path = path.clone();
                        let remove_entity = entity.clone();
                        chips = chips.child(
                            div()
                                .flex()
                                .flex_row()
                                .items_center()
                                .gap_1()
                                .rounded(px(4.0))
                                .bg(rgba(0x7f7f7f14))
                                .px_2()
                                .py_1()
                                .child(
                                    div()
                                        .text_size(sp(CAPTION_SIZE))
                                        .child(name),
                                )
                                .child(
                                    div()
                                        .id(format!("file-remove-{}", remove_path.display()))
                                        .cursor_pointer()
                                        .text_color(rgb(0xe8463a))
                                        .on_click(move |_, _, cx| {
                                            remove_entity.update(cx, |shell, cx| {
                                                shell.remove_file_ref(&remove_path, cx);
                                            });
                                        })
                                        .child("×"),
                                ),
                        );
                    }
                    input_box = input_box.child(chips);
                }
                input_box
                    .on_key_down(move |event, window, cx| {
                                        if std::env::var_os("QCODE_GUI_KEY_DEBUG").is_some() {
                                            eprintln!(
                                                "qcode-gui key: key={} platform={} secondary={} shift={}",
                                                event.keystroke.key,
                                                event.keystroke.modifiers.platform,
                                                event.keystroke.modifiers.secondary(cx),
                                                event.keystroke.modifiers.shift,
                                            );
                                        }
                                        if is_send_shortcut(&event.keystroke) {
                                            entity.update(cx, |shell, cx| {
                                                shell.send_prompt(window, cx);
                                            });
                                        }
                                    })
                                    .child(
                                        Textarea::new(&self.input)
                                            .bordered(false)
                                            .text_size(sp(15.0))
                                            .aria_label("Message qcode"),
                                    )
                            })
                            .child(
                                div()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .justify_between()
                                    .child(
                                        Button::new("upload")
                                            .label("选择文件")
                                            .on_click(cx.listener(|this, _, _window, cx| {
                                                this.upload_file(cx)
                                            })),
                                    )
                                    .child(
                                        // Round action button: a blue up-arrow sends
                                        // (queueing while a turn runs), a red dot
                                        // interrupts the running turn — shown only
                                        // when the turn is running and the draft is
                                        // empty, so a draft always keeps Send.
                                        if self.app.status.running && !composer_has_draft {
                                            div()
                                                .w(px(36.0))
                                                .h(px(36.0))
                                                .rounded(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .bg(rgba(0x0000000d))
                                                .hover(|style| style.bg(rgba(0x0000001a)))
                                                .id("interrupt")
                                                .on_click(cx.listener(|this, _, _window, cx| {
                                                    this.stop();
                                                    cx.notify();
                                                }))
                                                .child(
                                                    div()
                                                        .w(px(12.0))
                                                        .h(px(12.0))
                                                        .rounded(px(6.0))
                                                        .bg(rgb(0xe8463a)),
                                                )
                                        } else {
                                            div()
                                                .w(px(36.0))
                                                .h(px(36.0))
                                                .rounded(px(18.0))
                                                .flex()
                                                .items_center()
                                                .justify_center()
                                                .cursor_pointer()
                                                .bg(rgb(0x3b82f6))
                                                .hover(|style| style.bg(rgb(0x2563eb)))
                                                .id("send")
                                                .on_click(cx.listener(|this, _, window, cx| {
                                                    this.send_prompt(window, cx)
                                                }))
                                                .child(
                                                    Icon::new(IconName::ArrowUp)
                                                        .with_size(Size::Small)
                                                        .text_color(rgb(0xffffff)),
                                                )
                                        },
                                    ),
                            )
                    )
                        }
                    ),
            )
            .child(
                div()
                    .pl(px(MAIN_PAD_L))
                    .pr(px(MAIN_PAD_R))
                    .pb_2()
                    .child(
                        div()
                            .rounded(px(8.0))
                            .bg(popover)
                            .border_1()
                            .border_color(border)
                            .child(
                                StatusBar::new()
                                    .left(self.notice.clone().unwrap_or_default())
                                    .left(format!(
                                        "{}:{}",
                                        self.app.status.provider, self.app.status.model
                                    ))
                                    .left(self.cwd.clone())
                                    .right(context_label(&self.app))
                                    .right(format!("${:.4}", self.app.status.cost_usd))
                                    .right(format!("sandbox {}", self.app.sandbox))
                                    .right(if self.app.status.running {
                                        format!(
                                            "{} 等待回复…",
                                            WAITING_SPINNER[self.waiting_frame as usize]
                                        )
                                    } else {
                                        "空闲".to_string()
                                    }),
                            ),
                    ),
            )
    }
}
