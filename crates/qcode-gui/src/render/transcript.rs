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
    match shell.units.get(index).copied() {
        Some(DisplayUnit::Loose(block)) => block_row(block, shell, entity),
        Some(DisplayUnit::Turn(turn)) => turn_row(turn, shell, entity),
        None => {
            // The trailing scroller row holds the blinking typing indicator
            // while a turn streams; once it finishes the row is empty.
            if shell.app.status.running {
                running_row(shell, color, divider)
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
/// The QC mark as `currentColor`, so the running row follows the theme.
const QC_MARK: &[u8] = include_bytes!(concat!(
    env!("CARGO_MANIFEST_DIR"),
    "/../qcode/res/icons/src/qcode-mark-current.svg"
));

/// DSH-style running row at the tail of the transcript: a spinning QC mark,
/// then "工作中，用时 1分23秒 ···". The seconds are recomputed on every render,
/// driven by the 1s waiting ticker; the mark spins on its own animation.
pub(crate) fn running_row(shell: &Shell, color: Hsla, divider: Hsla) -> AnyElement {
    let elapsed = shell
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
            Animation::new(std::time::Duration::from_millis(1200)).repeat(),
            |icon, delta| icon.rotate(radians(delta * std::f32::consts::TAU)),
        );
    div()
        .flex()
        .flex_col()
        .w_full()
        .child(
            div()
                .h(px(0.5))
                .w_full()
                .bg(divider)
                .mt(px(8.0))
                .mb(px(10.0)),
        )
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

/// Chinese run duration: `23秒`, `1分23秒`, `1小时2分3秒` (DSH's format).
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
