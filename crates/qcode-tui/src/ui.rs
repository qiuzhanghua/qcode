//! Rendering: a pure function from [`App`] to a frame.
//!
//! Keeping layout and drawing separate from the event loop lets the whole
//! screen be rendered into `TestBackend` in tests, with no terminal required.

use ratatui::{
    Frame,
    layout::{Constraint, Flex, Layout, Rect},
    style::{Color, Modifier, Style},
    text::{Line, Span},
    widgets::{Block as UiBlock, Clear, Paragraph, Wrap},
};

use qcode_ui_state::{App, ApprovalPrompt, Block, ContextDisplay, ToolEntry};

/// Expanded tool output is capped so one huge result cannot dominate a frame.
const TOOL_OUTPUT_LINES: usize = 400;
/// Hard bound on the block window even when every block is a single line.
const MAX_WINDOW_BLOCKS: usize = 2_000;
use qcode_core::event::{ToolStatus, TurnStatus};
use qcode_core::todo::{TodoItem, TodoStatus};

pub fn render(frame: &mut Frame, app: &App) {
    // The composer grows with the number of lines, up to a third of the
    // screen, so multi-line prompts stay readable.
    let lines = app.textarea.lines().len() as u16;
    let max_composer = (frame.area().height / 3).max(3);
    let composer_height = (lines + 2).clamp(3, max_composer);

    let areas = Layout::vertical([
        Constraint::Length(1),
        Constraint::Min(3),
        Constraint::Length(composer_height),
    ])
    .split(frame.area());

    render_status(frame, app, areas[0]);
    render_transcript(frame, app, areas[1]);
    render_composer(frame, app, areas[2]);

    if let Some(prompt) = &app.approval {
        render_approval(frame, prompt, &app.sandbox);
    }
}

fn sandbox_style(sandbox: &str) -> Style {
    if sandbox == "none" {
        Style::default().fg(Color::DarkGray)
    } else {
        Style::default()
            .fg(Color::Green)
            .add_modifier(Modifier::BOLD)
    }
}

/// Estimated context usage, capped to 0–100%.
fn context_percent(app: &App) -> Option<f64> {
    let window = app.context_window.filter(|window| *window > 0)?;
    Some((app.context_tokens as f64 / window as f64 * 100.0).clamp(0.0, 100.0))
}

/// Wide terminals fit `ctx 15.4k/128k`; narrower ones fall back to a percent.
const CONTEXT_TOKENS_WIDTH: u16 = 100;

fn format_tokens(tokens: u64) -> String {
    if tokens < 1_000 {
        return tokens.to_string();
    }
    if tokens < 1_000_000 {
        let thousands = tokens as f64 / 1_000.0;
        if (thousands - thousands.round()).abs() < 0.05 {
            return format!("{thousands:.0}k");
        }
        return format!("{thousands:.1}k");
    }
    let millions = tokens as f64 / 1_000_000.0;
    if (millions - millions.round()).abs() < 0.05 {
        format!("{millions:.0}M")
    } else {
        format!("{millions:.1}M")
    }
}

fn context_label(app: &App, width: u16) -> String {
    let Some(window) = app.context_window.filter(|window| *window > 0) else {
        return "ctx —".to_string();
    };

    let absolute = match app.context_display {
        ContextDisplay::Auto => width >= CONTEXT_TOKENS_WIDTH,
        ContextDisplay::Percent => false,
        ContextDisplay::Tokens => true,
    };
    if absolute {
        format!(
            "ctx {}/{}",
            format_tokens(app.context_tokens),
            format_tokens(window)
        )
    } else {
        let percent = context_percent(app).unwrap_or(0.0);
        format!("ctx {percent:.0}%")
    }
}

fn context_style(app: &App) -> Style {
    match context_percent(app) {
        Some(percent) if percent >= 78.0 => Style::default().fg(Color::Red),
        Some(percent) if percent >= 50.0 => Style::default().fg(Color::Yellow),
        Some(_) => Style::default().fg(Color::Gray),
        None => Style::default().fg(Color::DarkGray),
    }
}

fn status_label(app: &App) -> (&'static str, Color) {
    if app.approval.is_some() {
        ("awaiting approval", Color::Yellow)
    } else if app.status.running {
        ("running", Color::Cyan)
    } else if app.quit_armed {
        ("ctrl-c again to quit", Color::Yellow)
    } else {
        match app.status.last {
            Some(TurnStatus::Completed) => ("completed", Color::Green),
            Some(TurnStatus::Cancelled) => ("cancelled", Color::Yellow),
            Some(TurnStatus::Failed) => ("failed", Color::Red),
            None => ("idle", Color::DarkGray),
        }
    }
}

fn render_status(frame: &mut Frame, app: &App, area: Rect) {
    let (label, color) = status_label(app);
    let muted = Style::default().fg(Color::Gray);
    let mut spans = vec![
        Span::styled(
            " qcode",
            Style::default()
                .fg(Color::Cyan)
                .add_modifier(Modifier::BOLD),
        ),
        Span::raw("  "),
        Span::styled(
            format!("{}:{}", app.status.provider, app.status.model),
            muted,
        ),
        Span::raw("  "),
        Span::styled(context_label(app, area.width), context_style(app)),
        Span::raw("  "),
        Span::styled(
            format!("sandbox {}", app.sandbox),
            sandbox_style(&app.sandbox),
        ),
        Span::raw("  "),
        Span::styled(format!("turn {}", app.status.turn), muted),
        Span::raw("  "),
        Span::styled(format!("${:.4}", app.status.cost_usd), muted),
        Span::raw("  "),
        Span::styled(
            label,
            Style::default().fg(color).add_modifier(Modifier::BOLD),
        ),
    ];
    if !app.collapsed_children.is_empty() {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            "child folded",
            Style::default().fg(Color::Yellow),
        ));
    }
    if app.scroll > 0 {
        spans.push(Span::raw("  "));
        spans.push(Span::styled(
            format!("↑{}", app.scroll),
            Style::default().fg(Color::Yellow),
        ));
    }
    frame.render_widget(Paragraph::new(Line::from(spans)), area);
}

fn render_transcript(frame: &mut Frame, app: &App, area: Rect) {
    let height = area.height as usize;
    // Only blocks near the visible window are turned into `Line`s. The budget
    // grows with the scroll offset, so paging up walks backwards through the
    // transcript without ever laying out all of it.
    let budget = height.saturating_add(app.scroll).saturating_add(8);
    let lines = transcript_lines(app, budget);

    let paragraph = Paragraph::new(lines).wrap(Wrap { trim: false });
    let total = paragraph.line_count(area.width);
    // Bottom-anchored: `scroll` counts rows up from the newest line.
    let offset = total
        .saturating_sub(height)
        .saturating_sub(app.scroll)
        .min(u16::MAX as usize);
    frame.render_widget(paragraph.scroll((offset as u16, 0)), area);
}

/// Child progress is grouped by the child session id and folded per group.
fn child_group(block: &Block) -> Option<&str> {
    match block {
        Block::Subagent { group, .. } => Some(group),
        Block::Tool(tool) if tool.child => tool.group.as_deref(),
        _ => None,
    }
}

fn block_hidden(app: &App, block: &Block) -> bool {
    child_group(block).is_some_and(|group| app.child_group_folded(group))
}

fn block_rows(block: &Block) -> usize {
    match block {
        Block::User(text)
        | Block::Assistant(text)
        | Block::Notice(text)
        | Block::Skill(text)
        | Block::Subagent { text, .. } => text.split('\n').count().max(1),
        Block::Todo(items) => 1 + items.len(),
        Block::Tool(tool) => {
            1 + if tool.expanded {
                tool.output
                    .split('\n')
                    .take(TOOL_OUTPUT_LINES)
                    .count()
                    .max(1)
            } else {
                0
            }
        }
    }
}

fn transcript_lines(app: &App, budget: usize) -> Vec<Line<'static>> {
    // Walk backwards from the newest block until the budget is met, then
    // restore reading order.
    let mut selected = Vec::new();
    let mut rows = 0;
    for block in app.blocks.iter().rev() {
        if block_hidden(app, block) {
            continue;
        }
        selected.push(block);
        rows += block_rows(block);
        if rows >= budget || selected.len() >= MAX_WINDOW_BLOCKS {
            break;
        }
    }
    selected.reverse();

    let mut lines = Vec::new();
    for block in selected {
        match block {
            Block::User(text) => {
                push_text(&mut lines, "❯ ", text, Style::default().fg(Color::Cyan))
            }
            Block::Assistant(text) => {
                push_text(&mut lines, "  ", text, Style::default());
            }
            Block::Notice(text) => {
                push_text(&mut lines, "· ", text, Style::default().fg(Color::DarkGray))
            }
            Block::Skill(text) => {
                push_text(&mut lines, "◆ ", text, Style::default().fg(Color::Magenta))
            }
            Block::Subagent { text, .. } => {
                push_text(&mut lines, "  ↳ ", text, Style::default().fg(Color::Blue))
            }
            Block::Todo(items) => push_todo(&mut lines, items),
            Block::Tool(tool) => {
                let nested = tool
                    .group
                    .as_deref()
                    .filter(|_| !tool.child)
                    .map(|group| (app.child_block_count(group), app.child_group_folded(group)));
                push_tool(&mut lines, tool, nested);
            }
        }
    }

    if !app.streaming.is_empty() {
        push_text(&mut lines, "  ", &app.streaming, Style::default());
        lines.push(Line::from(Span::styled(
            "▍",
            Style::default().fg(Color::Cyan),
        )));
    }
    lines
}

fn push_text(lines: &mut Vec<Line<'static>>, prefix: &'static str, text: &str, style: Style) {
    for (index, chunk) in text.split('\n').enumerate() {
        let lead = if index == 0 { prefix } else { "  " };
        lines.push(Line::from(vec![
            Span::styled(lead, style),
            Span::styled(chunk.to_string(), style),
        ]));
    }
}

fn status_glyph(status: Option<ToolStatus>) -> (&'static str, &'static str, Color) {
    match status {
        None => ("⏺", "running", Color::Cyan),
        Some(ToolStatus::Ok) => ("✔", "ok", Color::Green),
        Some(ToolStatus::Error) => ("✘", "error", Color::Red),
        Some(ToolStatus::Cancelled) => ("◼", "cancelled", Color::Yellow),
        Some(ToolStatus::Interrupted) => ("◼", "interrupted", Color::Magenta),
    }
}

fn push_tool(lines: &mut Vec<Line<'static>>, tool: &ToolEntry, nested: Option<(usize, bool)>) {
    let (glyph, status, color) = status_glyph(tool.status);
    let elapsed = tool
        .elapsed
        .map(|duration| format!(" · {}", format_duration(duration)))
        .unwrap_or_default();
    let mut spans = vec![
        Span::styled(
            format!("{}{glyph} ", if tool.child { "  ↳ " } else { " " }),
            Style::default().fg(color),
        ),
        Span::styled(
            tool.name.clone(),
            Style::default().add_modifier(Modifier::BOLD),
        ),
        Span::raw(" "),
        Span::styled(tool.summary.clone(), Style::default().fg(Color::Gray)),
        Span::raw(" "),
        Span::styled(format!("[{status}{elapsed}]"), Style::default().fg(color)),
    ];
    if let Some((count, folded)) = nested
        && count > 0
    {
        let label = if folded {
            format!(" · {count} nested (folded)")
        } else {
            format!(" · {count} nested")
        };
        spans.push(Span::styled(label, Style::default().fg(Color::DarkGray)));
    }
    lines.push(Line::from(spans));

    if !tool.expanded {
        return;
    }
    if tool.output.is_empty() {
        lines.push(Line::from(Span::styled(
            "    (no output yet)",
            Style::default().fg(Color::DarkGray),
        )));
        return;
    }
    let diff_tool = matches!(tool.name.as_str(), "apply_patch" | "edit_file");
    for (rendered, chunk) in tool.output.split('\n').enumerate() {
        if rendered == TOOL_OUTPUT_LINES {
            let total = tool.output.split('\n').count();
            lines.push(Line::from(Span::styled(
                format!(
                    "    │ … {} more lines (Ctrl-O to collapse)",
                    total - rendered
                ),
                Style::default().fg(Color::DarkGray),
            )));
            break;
        }
        let style = if diff_tool {
            diff_style(chunk)
        } else {
            Style::default().fg(Color::DarkGray)
        };
        lines.push(Line::from(Span::styled(format!("    │ {chunk}"), style)));
    }
}

/// Red/green colouring for unified diffs in apply_patch/edit_file output.
fn diff_style(line: &str) -> Style {
    if line.starts_with("+++") || line.starts_with("---") {
        let color = if line.starts_with("+++") {
            Color::Green
        } else {
            Color::Red
        };
        Style::default().fg(color).add_modifier(Modifier::BOLD)
    } else if line.starts_with("@@") {
        Style::default().fg(Color::Cyan)
    } else if line.starts_with('+') {
        Style::default().fg(Color::Green)
    } else if line.starts_with('-') {
        Style::default().fg(Color::Red)
    } else if line.starts_with("diff ") || line.starts_with("index ") {
        Style::default().add_modifier(Modifier::BOLD)
    } else {
        Style::default().fg(Color::DarkGray)
    }
}

fn push_todo(lines: &mut Vec<Line<'static>>, items: &[TodoItem]) {
    let done = items
        .iter()
        .filter(|item| item.status == TodoStatus::Completed)
        .count();
    lines.push(Line::from(vec![
        Span::styled(" ☰ ", Style::default().fg(Color::Blue)),
        Span::styled("todo", Style::default().add_modifier(Modifier::BOLD)),
        Span::raw(" "),
        Span::styled(
            format!("{done}/{} done", items.len()),
            Style::default().fg(Color::Gray),
        ),
    ]));

    for (index, item) in items.iter().enumerate() {
        let (glyph, color) = match item.status {
            TodoStatus::Completed => ("[x]", Color::DarkGray),
            TodoStatus::InProgress => ("[~]", Color::Cyan),
            TodoStatus::Pending => ("[ ]", Color::Gray),
        };
        let mut content_style = Style::default();
        if item.status == TodoStatus::Completed {
            content_style = content_style.fg(Color::DarkGray);
        }
        lines.push(Line::from(vec![
            Span::styled(
                format!("   {}. ", index + 1),
                Style::default().fg(Color::DarkGray),
            ),
            Span::styled(format!("{glyph} "), Style::default().fg(color)),
            Span::styled(item.content.clone(), content_style),
        ]));
    }
}

fn format_duration(duration: time::Duration) -> String {
    let millis = duration.whole_milliseconds().max(0);
    if millis < 1_000 {
        format!("{millis}ms")
    } else {
        format!("{:.1}s", duration.as_seconds_f64())
    }
}

fn render_composer(frame: &mut Frame, app: &App, area: Rect) {
    // tui-textarea draws its own block and cursor.
    frame.render_widget(&app.textarea, area);
}

fn render_approval(frame: &mut Frame, prompt: &ApprovalPrompt, sandbox: &str) {
    let area = centered(72, 60, frame.area());
    frame.render_widget(Clear, area);

    let label = Style::default().fg(Color::Gray);
    let lines = vec![
        Line::from(vec![
            Span::styled("tool    ", label),
            Span::raw(prompt.tool.clone()),
        ]),
        Line::from(vec![
            Span::styled("call    ", label),
            Span::raw(prompt.summary.clone()),
        ]),
        Line::from(vec![
            Span::styled("reason  ", label),
            Span::raw(prompt.reason.clone()),
        ]),
        Line::from(vec![
            Span::styled("rule    ", label),
            Span::raw(prompt.rule.clone().unwrap_or_else(|| "—".to_string())),
        ]),
        Line::from(vec![
            Span::styled("argv    ", label),
            Span::raw(prompt.argv.clone().unwrap_or_else(|| "—".to_string())),
        ]),
        Line::from(vec![
            Span::styled("cwd     ", label),
            Span::raw(prompt.cwd.clone().unwrap_or_else(|| "—".to_string())),
        ]),
        Line::from(vec![
            Span::styled("sandbox ", label),
            Span::raw(sandbox.to_string()),
        ]),
        Line::from(""),
        Line::from(vec![
            Span::styled(
                "[y] allow",
                Style::default()
                    .fg(Color::Green)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("     "),
            Span::styled(
                "[a] allow this tool for the session",
                Style::default()
                    .fg(Color::Cyan)
                    .add_modifier(Modifier::BOLD),
            ),
            Span::raw("     "),
            Span::styled(
                "[n] deny",
                Style::default().fg(Color::Red).add_modifier(Modifier::BOLD),
            ),
        ]),
    ];

    let paragraph = Paragraph::new(lines)
        .block(
            UiBlock::bordered()
                .title(" Approval ")
                .border_style(Style::default().fg(Color::Yellow)),
        )
        .wrap(Wrap { trim: false });
    frame.render_widget(paragraph, area);
}

fn centered(percent_x: u16, percent_y: u16, area: Rect) -> Rect {
    let vertical = Layout::vertical([
        Constraint::Percentage((100 - percent_y) / 2),
        Constraint::Percentage(percent_y),
        Constraint::Percentage((100 - percent_y) / 2),
    ])
    .flex(Flex::Center)
    .split(area);
    let horizontal = Layout::horizontal([
        Constraint::Percentage((100 - percent_x) / 2),
        Constraint::Percentage(percent_x),
        Constraint::Percentage((100 - percent_x) / 2),
    ])
    .flex(Flex::Center)
    .split(vertical[1]);
    horizontal[1]
}
