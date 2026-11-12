use super::*;

pub(crate) enum MarkdownSegment {
    Text(String),
    Code { language: String, body: String },
}

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
                // Pending 单元由 `Shell::pending_units` 在排序后追加，不会
                // 进入这个分支；占位保持 match 穷尽。
                DisplayUnit::Pending { .. } => 2u8,
            },
        )
    });
    units.into_iter().map(|(_, unit)| unit).collect()
}
pub(crate) fn unit_row(
    index: usize,
    shell: &Shell,
    entity: &Entity<Shell>,
    color: Hsla,
    divider: Hsla,
) -> AnyElement {
    match shell.ui.units.get(index).cloned() {
        Some(DisplayUnit::Loose(block)) => block_row(block, shell, entity),
        Some(DisplayUnit::Turn(turn)) => turn_row(turn, shell, entity),
        Some(DisplayUnit::Pending { text, steered, .. }) => pending_row(text, steered),
        None => {
            // The trailing scroller row holds the blinking typing indicator
            // while a turn streams; once it finishes the row is empty.
            if shell.ui.app.status.running {
                running_row(shell, color, divider)
            } else {
                div().into_any_element()
            }
        }
    }
}

/// Pending 气泡：排队中或已推送、但引擎还没认领的消息。已推送的按已发送
/// 的用户消息同款样式渲染（dsh 式回显），让用户感觉问题已经发出去了；
/// 排队的保持低调的「排队中」样式。引擎认领后该单元随 queue/steering 状态
/// 消失，由真实事件渲染的 turn 卡替换。
fn pending_row(text: String, steered: bool) -> AnyElement {
    if steered {
        // 与真实用户消息（Block::User）同款气泡：右对齐、正文色，不再
        // 显示「等待下一步」——推送后这条消息已经属于运行中的对话。
        return div()
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
                    .child(text),
            )
            .into_any_element();
    }
    let preview: String = text.replace('\n', " ").chars().take(120).collect();
    let status = "排队中";
    // 与 queue dock 的配色保持一致：队列灰色。
    let (status_color, bg) = (rgb(0x888888), rgba(0x7f7f7f10));
    div()
        .flex()
        .flex_col()
        .w_full()
        .px_6()
        .py_1()
        .child(
            div()
                .w_full()
                .rounded(px(8.0))
                .border_1()
                .border_color(rgba(0x7f7f7f20))
                .bg(bg)
                .px_3()
                .py_2()
                .flex()
                .flex_col()
                .gap_1()
                .child(
                    div()
                        .text_size(sp(BODY_SIZE))
                        .text_color(rgb(0x999999))
                        .child(preview),
                )
                .child(
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(status_color)
                        .child(status),
                ),
        )
        .into_any_element()
}
pub(crate) fn conversation_icon(name: IconName) -> Icon {
    Icon::new(name)
        .with_size(Size::Small)
        .text_color(rgb(0x888888))
}
/// Lead of a foldable row: chevron, kind icon and label in one 8px group.
///
/// The row that hosts it uses a 4px gap, so the "· detail" after the label
/// reads as a single space while the icons stay clearly apart from the text —
/// the rhythm the reference design uses for 思考 and 运行命令 rows.
fn header_lead(expanded: bool, icon: IconName, label: impl Into<SharedString>) -> Div {
    div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .child(conversation_icon(if expanded {
            IconName::ChevronDown
        } else {
            IconName::ChevronRight
        }))
        .child(conversation_icon(icon))
        .child(label.into())
}
/// One diff row, so an inline card can be sized without a layout pass.
const DIFF_ROW_HEIGHT: f32 = 20.0;

/// The QC mark as `currentColor`, so the running row follows the theme.
const QC_MARK: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../qcode/res/icons/src/qcode-mark-current.svg"
));

/// Running row at the tail of the transcript: a flipping QC mark,
/// then "工作中，用时 1分23秒 ···". The seconds are recomputed on every render,
/// driven by the 1s waiting ticker; the mark flips on its own animation.
pub(crate) fn running_row(shell: &Shell, color: Hsla, divider: Hsla) -> AnyElement {
    let elapsed = shell
        .ui
        .app
        .turns
        .last()
        .filter(|turn| turn.running())
        .map(|turn| (time::OffsetDateTime::now_utc() - turn.started_at).max(time::Duration::ZERO));
    let label = match elapsed {
        Some(elapsed) => format!("工作中，用时 {} ···", format_duration_zh(elapsed)),
        None => "工作中".to_string(),
    };
    let icon = Icon::default()
        .data(QC_MARK)
        .size(px(14.0))
        .text_color(color)
        .with_animation(
            "qc-running",
            Animation::new(std::time::Duration::from_millis(2400)).repeat(),
            move |icon, delta| {
                let angle = flip_angle(delta);
                let scale = angle.cos();
                // Non-physical depth cues: the back face is dimmed, and the
                // mark sways towards the side turning towards the viewer (a
                // yaw about the centre alone would not move it). Positive
                // sin shifts right, so the right edge leads.
                let alpha = if scale < 0.0 { 0.72 } else { 1.0 };
                let dx = angle.sin() * 1.5;
                icon.text_color(color.opacity(alpha)).transform(
                    Transformation::default()
                        .with_scaling(size(scale, 1.0))
                        .with_translation(point(px(dx), px(0.0))),
                )
            },
        );
    div()
        .flex()
        .flex_col()
        .w_full()
        .child(div().h(px(0.5)).mx_6().bg(divider).mt(px(8.0)).mb(px(10.0)))
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_1()
                .px_6()
                .py_1()
                .child(icon)
                .child(div().text_size(sp(12.0)).text_color(color).child(label)),
        )
        .into_any_element()
}

/// One 2.4s cycle: two consecutive face flips (0 -> 2π in 1.6s), then a
/// 0.8s hold facing the viewer. The mark is a flat plane; a yaw about its
/// vertical axis projects as a horizontal scale of `cos`, which turns
/// negative on the back half and mirrors the mark. gpui has no 3D transform,
/// so this is an orthographic flip; the sway and dimming in the animator are
/// non-physical cues that make the direction readable (the right edge leads).
fn flip_angle(delta: f32) -> f32 {
    const FLIP: f32 = 2.0 / 3.0;
    let smooth = |t: f32| t * t * (3.0 - 2.0 * t);
    if delta < FLIP {
        std::f32::consts::TAU * smooth(delta / FLIP)
    } else {
        std::f32::consts::TAU
    }
}

/// Chinese run duration: `23秒`, `1分23秒`, `1小时2分3秒`.
pub(crate) fn format_duration_zh(duration: time::Duration) -> String {
    let total = duration.whole_seconds().max(0) as u64;
    let hours = total / 3600;
    let minutes = total / 60 % 60;
    let seconds = total % 60;
    if hours > 0 {
        format!("{hours}小时{minutes}分{seconds}秒")
    } else if total >= 60 {
        format!("{minutes}分{seconds}秒")
    } else {
        format!("{seconds}秒")
    }
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
    let preview = reasoning_preview(&shell.ui.app.reasoning_streaming);
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
            .gap_1()
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
            .child(header_lead(expanded, IconName::Brain, "思考"))
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
                    .child(shell.ui.app.reasoning_streaming.clone()),
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
    let streaming =
        shell.ui.app.status.running && shell.ui.app.streaming_reasoning_block == Some(block);
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
            .gap_1()
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
            .child(header_lead(expanded, IconName::Brain, "思考"))
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
    let turn = &shell.ui.app.turns[index];
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
        .filter(|block| !matches!(shell.ui.app.blocks.get(*block), Some(Block::Notice(_))))
        .collect();
    let has_process = !process_blocks.is_empty()
        || (turn.running() && !shell.ui.app.reasoning_streaming.is_empty());
    let show_process = !shell.ui.collapse_all;

    if has_process && show_process {
        // One container for the whole execution process (thinking + tools).
        // It opens with the turn and the header toggles it; once the turn
        // finishes it auto-folds and the answer renders below, outside the
        // container.
        let process_open = shell.ui.process_open_turns.contains(&index);
        let mut process = div().flex().flex_col().gap_0().w_full().items_start();
        for block in &process_blocks {
            if let Block::Reasoning(entry) = &shell.ui.app.blocks[*block] {
                if entry.text.is_empty()
                    && turn.running()
                    && !shell.ui.app.reasoning_streaming.is_empty()
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
            // Amber, not "failed": a budget pause is a hand-off (ADR-0060).
            Some(qcode_core::event::TurnStatus::Paused) => "paused",
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
        let process_title = process_label(shell, &process_blocks)
            .unwrap_or_else(|| format!("执行过程 · {} 项", process_blocks.len()));
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
            .child(process_title)
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
        // tools while the first words arrive, and `answer_block` is set as
        // soon as text starts streaming.
        let has_answer = !turn.running() && turn.answer_block.is_some();
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

    if shell.pause_card_visible(index)
        && let Some(pause) = pause_card(&shell.ui.app, index, shell, entity)
    {
        card = card.child(pause);
    }

    if let Some(block) = turn.answer_block {
        card = card.child(block_row(block, shell, entity));
    } else if turn.running() && !shell.ui.app.streaming.is_empty() {
        card = card.child(
            div()
                .px_6()
                .py_2()
                .text_size(sp(ANSWER_SIZE))
                .line_height(sp(ANSWER_LINE))
                .child(
                    TextView::markdown(
                        format!("streaming-turn-{index}"),
                        shell.ui.app.streaming.clone(),
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
/// The command of a shell call as its own card: a light background, a status
/// dot on the first line, a `$` gutter on every line, and a copy button in the
/// top-right corner.
fn command_card(
    shell: &Shell,
    entity: &Entity<Shell>,
    tool: &qcode_ui_state::ToolEntry,
    command: &str,
    index: usize,
) -> AnyElement {
    let dot = match tool.status {
        Some(ToolStatus::Ok) => rgb(0x4c9a68),
        Some(ToolStatus::Error) => rgb(0xc26464),
        Some(ToolStatus::Cancelled) | Some(ToolStatus::Interrupted) => rgb(0xb08a4a),
        // Still running: the row keeps the neutral colour until it finishes.
        None => rgb(0xa8a8a8),
    };
    let mut lines = div().flex().flex_col().w_full().pr(px(52.0));
    for (line_index, line) in command.lines().enumerate() {
        // Only the first line carries the status dot; the rest keep the
        // gutter so the `$` column stays straight.
        let gutter = if line_index == 0 {
            div()
                .flex_none()
                .w(px(8.0))
                .h(px(8.0))
                .rounded_full()
                .bg(dot)
                .into_any_element()
        } else {
            div().flex_none().w(px(8.0)).into_any_element()
        };
        lines = lines.child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .w_full()
                .font_family(shell.mono_font.clone())
                .text_size(sp(RESULT_SIZE))
                .line_height(sp(RESULT_LINE))
                .child(gutter)
                .child(div().flex_none().text_color(rgb(0x9a9a9a)).child("$"))
                .child(
                    div()
                        .flex_1()
                        .min_w_0()
                        .text_color(rgb(0x333333))
                        .child(line.to_string()),
                ),
        );
    }
    div()
        .relative()
        .w_full()
        .rounded(px(8.0))
        .border_1()
        .border_color(rgb(0xececec))
        .bg(rgb(0xf7f7f7))
        .p(px(12.0))
        .child(lines)
        .child(
            div()
                .absolute()
                .top(px(6.0))
                .right(px(8.0))
                .child(copy_command_button(
                    entity,
                    &tool.call_id,
                    command,
                    index,
                    shell,
                )),
        )
        .into_any_element()
}

/// 「复制」 for a shell command. Copies the raw command (not the rendered
/// lines, so the `$` gutters stay out of the clipboard) and flips to「已复制」
/// for a moment afterwards.
fn copy_command_button(
    entity: &Entity<Shell>,
    call_id: &str,
    command: &str,
    index: usize,
    shell: &Shell,
) -> AnyElement {
    copy_chip(
        SharedString::from(format!("copy-command-{index}")),
        call_id.to_string(),
        "复制",
        command.to_string(),
        shell.copied_recently(call_id),
        entity.clone(),
    )
}

/// One copy chip: the shape every「复制」button in the transcript shares, so a
/// command, a code block, an answer and a whole turn all look and behave the
/// same way.
///
/// `flash` is the id the shell remembers for a moment after a click; that is
/// what flips the label to「已复制」. `text` is the finished clipboard payload —
/// the chip never decides *what* to copy, so the choice stays visible at each
/// call site (and in the pure functions of [`crate::copy_text`]).
fn copy_chip(
    element_id: SharedString,
    flash: String,
    label: &'static str,
    text: String,
    copied: bool,
    entity: Entity<Shell>,
) -> AnyElement {
    div()
        .id(element_id)
        .cursor_pointer()
        .rounded(px(5.0))
        .px_2()
        .py_1()
        .text_size(sp(CAPTION_SIZE))
        .text_color(if copied { rgb(0x4c9a68) } else { rgb(0x8a8a8a) })
        .hover(|style| style.bg(rgba(0x7f7f7f14)).text_color(rgb(0x333333)))
        .child(if copied { "已复制" } else { label })
        .on_click(move |_, _, cx| {
            cx.write_to_clipboard(ClipboardItem::new_string(text.clone()));
            entity.update(cx, |shell, cx| shell.mark_copied(flash.clone(), cx));
        })
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
                let expanded = shell.ui.expanded_code.contains(&key);
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
                let flash = format!("code-{key}");
                let mut code = TextView::markdown(format!("assistant-code-{key}"), fence)
                    .code_block_actions(code_block_copy_actions(
                        flash.clone(),
                        language.clone(),
                        // The whole block, not `visible`: the fold is a
                        // reading aid, and a copy button that quietly hands
                        // back twelve of forty lines would be a trap. (A
                        // drag-selection is the opposite trade — it copies
                        // what is on screen; see `copy_selection`.)
                        body.clone(),
                        shell.copied_recently(&flash),
                        entity.clone(),
                    ))
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
    column = column.child(answer_actions(index, text, shell, entity));
    column.into_any_element()
}

/// The copy affordances shown inline at a code block's top-right.
///
/// Upstream only suppresses its language label when this hook is set
/// (`TextBlock` paints the language there otherwise), so the hook draws the
/// language itself and puts the chip beside it.
///
/// `copied` is read here, at build time, and not inside the closure: the
/// closure runs while the view tree is being laid out, and reading the shell's
/// own entity then is exactly the borrow the render pass is holding.
fn code_block_copy_actions(
    flash: String,
    language: String,
    body: String,
    copied: bool,
    entity: Entity<Shell>,
) -> impl Fn(&gpui_kit::base::text::CodeBlock, &mut Window, &mut gpui_kit::App) -> AnyElement
+ Send
+ Sync
+ 'static {
    move |_, _, _| {
        div()
            .flex()
            .flex_row()
            .items_center()
            .gap_1()
            .when(!language.is_empty(), |this| {
                this.child(
                    div()
                        .px_1()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(rgb(0x999999))
                        .child(language.clone()),
                )
            })
            .child(copy_chip(
                SharedString::from(format!("copy-{flash}")),
                flash.clone(),
                "复制",
                body.clone(),
                copied,
                entity.clone(),
            ))
            .into_any_element()
    }
}

/// The copy chips under an answer: plain text, the markdown source, and — when
/// the answer belongs to a turn — the whole turn (question included).
///
/// The plain-text chip is the default because that is what survives a paste
/// into a chat window or a document; `Markdown` is there for the other errand,
/// pasting an answer back into something that reads markdown.
fn answer_actions(index: usize, text: &str, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    if text.trim().is_empty() {
        return div().into_any_element();
    }
    // Converted on every frame the row is rendered. The transcript is
    // virtualized, so that is a handful of visible answers, not the whole
    // session — and caching the conversion would have to be invalidated as the
    // answer streams.
    let plain = crate::copy_text::plain_text_from_markdown(text);
    let source = text.trim();
    let source_differs = plain != source;
    let plain_flash = format!("answer-{index}");
    let markdown_flash = format!("answer-md-{index}");
    let mut row = div()
        .flex()
        .flex_row()
        .items_center()
        .justify_start()
        .gap_1()
        .mb_1();
    if !plain.is_empty() {
        row = row.child(copy_chip(
            SharedString::from(format!("copy-{plain_flash}")),
            plain_flash.clone(),
            "复制",
            plain,
            shell.copied_recently(&plain_flash),
            entity.clone(),
        ));
    }
    // `Markdown` only when it differs from the plain text: an answer with no
    // marks would otherwise offer two buttons that put the same string on the
    // clipboard.
    if source_differs {
        row = row.child(copy_chip(
            SharedString::from(format!("copy-{markdown_flash}")),
            markdown_flash.clone(),
            "Markdown",
            source.to_string(),
            shell.copied_recently(&markdown_flash),
            entity.clone(),
        ));
    }
    if let Some((turn_index, whole)) = turn_copy_text_for(shell, index) {
        let flash = format!("turn-{turn_index}");
        row = row.child(copy_chip(
            SharedString::from(format!("copy-{flash}")),
            flash.clone(),
            "整轮",
            whole,
            shell.copied_recently(&flash),
            entity.clone(),
        ));
    }
    row.into_any_element()
}

/// The turn whose final answer is this block, plus the plain text of the whole
/// turn (question then answer).
///
/// The answer's own block index is the only handle `block_row` has, so the turn
/// is found by searching for the turn that claims it. That costs one linear
/// scan per assistant block per frame, which is cheaper than threading a turn
/// index through `block_row` and every one of its callers.
fn turn_copy_text_for(shell: &Shell, answer_block: usize) -> Option<(usize, String)> {
    let turn_index = shell
        .ui
        .app
        .turns
        .iter()
        .position(|turn| turn.answer_block == Some(answer_block))?;
    let turn = &shell.ui.app.turns[turn_index];
    let question = turn
        .input_block
        .and_then(|block| shell.ui.app.blocks.get(block))
        .and_then(|block| match block {
            Block::User { text, .. } => Some(text.as_str()),
            _ => None,
        });
    let answer = shell
        .ui
        .app
        .blocks
        .get(answer_block)
        .and_then(|block| match block {
            Block::Assistant(text) => Some(text.as_str()),
            _ => None,
        });
    let whole = crate::copy_text::turn_copy_text(question, answer);
    (!whole.is_empty()).then_some((turn_index, whole))
}

pub(crate) fn block_row(index: usize, shell: &Shell, entity: &Entity<Shell>) -> AnyElement {
    if index >= shell.ui.app.blocks.len() {
        return div()
            .px_4()
            .py_2()
            .child(
                TextView::markdown(format!("streaming-{index}"), shell.ui.app.streaming.clone())
                    .code_block_actions(|_, _, _| div().into_any_element()),
            )
            .into_any_element();
    }

    match &shell.ui.app.blocks[index] {
        Block::User {
            text,
            images,
            files,
            scheduled,
        } => {
            // A scheduled-task delivery renders as its own card instead of a
            // typed-message bubble: header strip with the wall-clock time, then
            // the task title and prompt (ADR-0059).
            if let Some(scheduled) = scheduled {
                return div()
                    .flex()
                    .flex_col()
                    .w_full()
                    .px_6()
                    .py_2()
                    .child(
                        div()
                            .w_full()
                            .rounded(px(10.0))
                            .border_1()
                            .border_color(rgba(0x7f7f7f20))
                            .bg(rgb(0xffffff))
                            .overflow_hidden()
                            .flex()
                            .flex_col()
                            .child(
                                div()
                                    .bg(rgb(0xf1f5f9))
                                    .px_3()
                                    .py_1()
                                    .flex()
                                    .flex_row()
                                    .items_center()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(sp(CAPTION_SIZE))
                                            .text_color(rgb(0x555555))
                                            .child(format!("⏱ 定时任务 · {}", scheduled.at)),
                                    ),
                            )
                            .child(
                                div()
                                    .px_3()
                                    .py_2()
                                    .flex()
                                    .flex_col()
                                    .gap_1()
                                    .child(
                                        div()
                                            .text_size(sp(BODY_SIZE))
                                            .line_height(sp(BODY_LINE))
                                            .font_weight(FontWeight::SEMIBOLD)
                                            .text_color(rgb(0x1a1a1a))
                                            .child(scheduled.title.clone()),
                                    )
                                    .child(
                                        TextView::markdown(
                                            format!("scheduled-{index}"),
                                            scheduled.prompt.clone(),
                                        )
                                        .code_block_actions(|_, _, _| div().into_any_element())
                                        .text_size(sp(BODY_SIZE))
                                        .line_height(sp(BODY_LINE)),
                                    ),
                            ),
                    )
                    .into_any_element();
            }
            let mut column = div().flex().flex_col().items_end().gap_2();
            column = column.child(
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
            );
            // Pictures sit under the question, inside the same bubble column, so
            // folding the execution process can never hide them.
            for image in images {
                column = column.child(image_tile(image, shell, index));
            }
            // Files get a chip line: the bytes live in the store, so the name
            // and size are all the transcript needs.
            for (slot, file) in files.iter().enumerate() {
                column = column.child(
                    div()
                        .flex()
                        .flex_row()
                        .items_center()
                        .gap_1()
                        .rounded(px(6.0))
                        .bg(rgba(0x7f7f7f14))
                        .px_2()
                        .py_1()
                        .child(div().text_size(sp(CAPTION_SIZE)).child(file.name.clone()))
                        .child(
                            div()
                                .text_size(sp(CAPTION_SIZE))
                                .text_color(rgb(0x999999))
                                .child(human_bytes(file.bytes)),
                        )
                        .id(("user-file", index * 32 + slot)),
                );
            }
            div()
                .flex()
                .w_full()
                .justify_end()
                .px_6()
                .py_2()
                .child(column)
                .into_any_element()
        }
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
                .gap_1()
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
                .child(header_lead(expanded, tool_icon_name(&tool.name), label))
                .children((!summary.is_empty()).then(|| {
                    // Truncate the detail, never the counts: a long command or
                    // path must not push the +N −D off the row.
                    div()
                        .max_w(px(420.0))
                        .overflow_hidden()
                        .text_ellipsis()
                        .text_color(rgb(0x777777))
                        .child(format!("· {summary}"))
                }))
                .children(tool.diff.as_ref().map(diff_chips));
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
                // The change itself, above the command/output box.
                if let Some(state) = shell.inline_diff(&tool.call_id) {
                    // The viewer fills its parent, so the card needs a height:
                    // one row per patch line, capped so a huge rewrite does not
                    // push the rest of the turn off screen.
                    let rows = tool
                        .diff
                        .as_ref()
                        .map(|diff| diff.patch.lines().count())
                        .unwrap_or(0);
                    let height = (rows as f32 * DIFF_ROW_HEIGHT).clamp(96.0, 320.0);
                    details = details.child(
                        div()
                            .w_full()
                            .h(px(height))
                            .rounded(px(6.0))
                            .overflow_hidden()
                            .border_1()
                            .border_color(rgba(0x7f7f7f20))
                            .child(Diff::new(&state)),
                    );
                }
                let command = tool
                    .arguments
                    .get("command")
                    .and_then(|value| value.as_str())
                    .map(str::to_string);
                let output_line_count = tool.output.lines().count();
                let foldable = output_line_count > 20;
                let output_key = format!("{index}:output");
                let output_expanded = shell.ui.expanded_code.contains(&output_key);
                let output_visible = if output_expanded || !foldable {
                    tool.output.clone()
                } else {
                    tool.output.lines().take(12).collect::<Vec<_>>().join("\n")
                };
                // The command gets its own card so the block reads like a
                // shell transcript: light background, a status dot on the
                // first line, a `$` gutter on every line, and a copy button.
                if let Some(command) = &command {
                    details = details.child(command_card(shell, entity, tool, command, index));
                }
                if !tool.output.is_empty() {
                    // Long output keeps square bottom corners so the fold
                    // toggle below reads as one piece with it.
                    let mut content = div()
                        .w_full()
                        .rounded(px(6.0))
                        .bg(rgba(0x7f7f7f08))
                        .border_1()
                        .border_color(rgba(0x7f7f7f20))
                        .p_2()
                        .flex()
                        .flex_col();
                    if foldable {
                        content = content.rounded_b(px(0.0));
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
                        // Plain multi-line output: markdown hard breaks (two
                        // trailing spaces) keep every line break, so the text
                        // renders exactly like the original one box — no
                        // nested code-block frame, same size and colour —
                        // while staying selectable.
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
                if tool
                    .diff
                    .as_ref()
                    .is_some_and(|diff| !diff.patch.is_empty())
                    && !shell.diff_broken(&tool.call_id)
                {
                    let call_id = tool.call_id.clone();
                    details = details.child(
                        div().flex().flex_row().pt_1().child(
                            Button::new(SharedString::from(format!("view-diff-{index}")))
                                .label("查看")
                                .on_click({
                                    let entity = entity.clone();
                                    move |_, _, cx| {
                                        let call_id = call_id.clone();
                                        entity.update(cx, |shell, cx| {
                                            shell.open_diff_view(&call_id, cx);
                                        });
                                    }
                                }),
                        ),
                    );
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

/// The `1KB +1 -1` chips after a tool row's summary.
fn diff_chips(diff: &ToolDiff) -> Div {
    let additions: u32 = diff.files.iter().map(|file| file.additions).sum();
    let deletions: u32 = diff.files.iter().map(|file| file.deletions).sum();
    let bytes: u64 = diff.files.iter().map(|file| file.bytes).sum();
    let mut row = div().flex().flex_row().items_center().gap_1();
    if bytes > 0 && additions > 0 && deletions == 0 {
        row = row.child(div().text_color(rgb(0x999999)).child(human_bytes(bytes)));
    }
    if additions > 0 {
        row = row.child(
            div()
                .text_color(rgb(0x4c9a68))
                .child(format!("+{additions}")),
        );
    }
    if deletions > 0 {
        row = row.child(
            div()
                .text_color(rgb(0xc26464))
                .child(format!("-{deletions}")),
        );
    }
    row
}

/// Byte counts as the diff header writes them: `842B`, `1.2KB`, `3.4MB`.
pub(crate) fn human_bytes(bytes: u64) -> String {
    const KB: f64 = 1024.0;
    let value = bytes as f64;
    if value < KB {
        format!("{bytes}B")
    } else if value < KB * KB {
        format!("{:.0}KB", value / KB)
    } else {
        format!("{:.1}MB", value / (KB * KB))
    }
}

/// What a turn did, the way the reference UI summarises it: "修改了文件，执行了
/// 命令，已读取文件". `None` when nothing falls into one of those buckets, so
/// the caller can fall back to counting items.
fn process_label(shell: &Shell, blocks: &[usize]) -> Option<String> {
    let mut writes = false;
    let mut shells = false;
    let mut reads = false;
    for block in blocks {
        let Some(Block::Tool(tool)) = shell.ui.app.blocks.get(*block) else {
            continue;
        };
        match tool.name.as_str() {
            "apply_patch" | "edit_file" | "write_file" => writes = true,
            "run_shell" => shells = true,
            "read_file" | "list_dir" | "grep" | "glob" => reads = true,
            _ => {}
        }
    }
    let mut parts = Vec::new();
    if writes {
        parts.push("修改了文件");
    }
    if shells {
        parts.push("执行了命令");
    }
    if reads {
        parts.push("已读取文件");
    }
    (!parts.is_empty()).then(|| parts.join("，"))
}

/// The pause card: why the turn stopped, what it got done, and the three ways
/// out. Amber rather than red — nothing failed (ADR-0060 §2.7).
fn pause_card(
    app: &qcode_ui_state::App,
    index: usize,
    shell: &Shell,
    entity: &Entity<Shell>,
) -> Option<AnyElement> {
    let card = qcode_ui_state::PauseCard::of(app, index)?;
    let amber = rgb(0xd9a441);
    let muted = rgba(0x7f7f7fcc);
    let mut body = div()
        .flex()
        .flex_col()
        .gap_1()
        .px_6()
        .py_2()
        .w_full()
        .child(
            div()
                .flex()
                .flex_row()
                .items_center()
                .gap_2()
                .child(
                    div()
                        .text_size(sp(BODY_SIZE))
                        .text_color(amber)
                        .child(format!("⏸ 已暂停 · {}", card.headline)),
                )
                .children(card.window_note.map(|note| {
                    div()
                        .text_size(sp(CAPTION_SIZE))
                        .text_color(muted)
                        .child(note)
                })),
        );
    if let Some(files) = card.files {
        body = body.child(
            div()
                .text_size(sp(SECONDARY_SIZE))
                .text_color(muted)
                .child(files),
        );
    }
    if let Some(summary) = card.summary {
        body = body.child(
            TextView::markdown(format!("pause-summary-{index}"), summary)
                .text_size(sp(BODY_SIZE))
                .line_height(sp(BODY_LINE)),
        );
    }
    for failure in card.failures {
        body = body.child(
            div()
                .text_size(sp(SECONDARY_SIZE))
                .text_color(rgb(0xd05050))
                .child(format!("✗ {failure}")),
        );
    }
    if !card.commands.is_empty() {
        let runs: Vec<&str> = card
            .commands
            .iter()
            .map(|line| line.command.as_str())
            .collect();
        let mut line = format!("已运行：{}", runs.join(" · "));
        if card.more_commands > 0 {
            line.push_str(&format!(" 等 {} 条", runs.len() + card.more_commands));
        }
        body = body.child(
            div()
                .text_size(sp(SECONDARY_SIZE))
                .text_color(muted)
                .child(line),
        );
    }
    let buttons = div()
        .flex()
        .flex_row()
        .items_center()
        .gap_2()
        .pt_1()
        .child(
            Button::new(("pause-continue", index))
                .label("继续")
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |shell, cx| shell.continue_paused_turn(cx));
                    }
                }),
        )
        .child(Button::new(("pause-wrap", index)).label("收尾").on_click({
            let entity = entity.clone();
            move |_, _, cx| {
                entity.update(cx, |shell, cx| shell.wrap_up_paused_turn(cx));
            }
        }))
        .child(
            Button::new(("pause-dismiss", index))
                .label("先这样")
                .on_click({
                    let entity = entity.clone();
                    move |_, _, cx| {
                        entity.update(cx, |shell, cx| {
                            shell.dismiss_pause();
                            cx.notify();
                        });
                    }
                }),
        )
        .children((shell.ui.app.status.window > 1).then(|| {
            div()
                .text_size(sp(CAPTION_SIZE))
                .text_color(muted)
                .child(format!("第 {} 个预算窗口", shell.ui.app.status.window))
        }));
    Some(
        div()
            .mx_6()
            .my_2()
            .rounded(px(8.0))
            .border_1()
            .border_color(rgba(0x7f7f7f30))
            .bg(rgba(0xd9a44112))
            .child(body.child(buttons))
            .into_any_element(),
    )
}

/// One attached picture: the bytes are in the attachment store, the ref only
/// names them.
fn image_tile(image: &qcode_ui_state::ImageBlock, shell: &Shell, index: usize) -> AnyElement {
    let Some(id) = qcode_attachments::AttachmentId::from_hex(&image.id) else {
        return div().into_any_element();
    };
    let path = qcode_attachments::AttachmentStore::object_path_in(&shell.data_dir, id);
    // A wiped data directory or a GC pass can remove the object; say so rather
    // than drawing an empty box.
    if !path.exists() {
        return div()
            .text_size(sp(SECONDARY_SIZE))
            .text_color(rgb(0x999999))
            .child("[图片已不可用]")
            .into_any_element();
    }
    // Fit inside a 360×260 box, never scaling up past the original.
    let (width, height) = (image.width.max(1) as f32, image.height.max(1) as f32);
    let scale = (360.0 / width).min(260.0 / height).min(1.0);
    div()
        .id(("image", index))
        .w(px((width * scale).max(1.0)))
        .h(px((height * scale).max(1.0)))
        .rounded(px(12.0))
        .overflow_hidden()
        .border_1()
        .border_color(rgba(0x7f7f7f33))
        .child(img(path).object_fit(ObjectFit::Cover).w_full().h_full())
        .into_any_element()
}
