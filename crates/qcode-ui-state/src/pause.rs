//! The pause card: everything a paused turn needs to show, derived from the
//! turn's own rows.
//!
//! Both frontends render the same card, so the shape lives here instead of in
//! either view. It is deliberately computed from blocks the state already has:
//! a replayed log shows the same card live (ADR-0060 §2.4), and no frontend has
//! to keep a second copy of the event stream.

use qcode_core::event::{PauseInfo, ToolDiff, ToolStatus};

use crate::{App, Block, ToolEntry};

/// How many command lines the card lists before summarising the rest.
const COMMAND_LIMIT: usize = 5;
const FAILURE_LIMIT: usize = 3;
/// One failed-output line is a reminder, not a log.
const DETAIL_LIMIT: usize = 120;

/// One row in the card's "what it ran" list.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct PauseCommand {
    pub command: String,
    pub failed: bool,
}

/// The card behind a paused turn.
#[derive(Debug, Clone, PartialEq, Eq, Default)]
pub struct PauseCard {
    /// The model's own "done / remaining / next" hand-off, when it wrote one.
    pub summary: Option<String>,
    /// `修改了 3 个文件（+120 −8）`, honest even when the summary call failed.
    pub files: Option<String>,
    pub commands: Vec<PauseCommand>,
    /// Commands beyond [`COMMAND_LIMIT`].
    pub more_commands: usize,
    /// `bash: exit 1 — …` style notes; usually why the task is unfinished.
    pub failures: Vec<String>,
    /// `已用 64/64 轮` / `成本 $5.00/$5.00`.
    pub headline: String,
    /// `第 2 个预算窗口`, when this is not the first window.
    pub window_note: Option<String>,
}

impl PauseCard {
    /// None when the turn did not pause, so callers can `if let` instead of
    /// checking the status first.
    pub fn of(app: &App, index: usize) -> Option<Self> {
        let turn = app.turns.get(index)?;
        let pause = turn.pause.as_ref()?;
        let mut card = Self {
            summary: pause.summary.clone(),
            headline: headline(pause),
            window_note: (pause.window > 1).then(|| format!("第 {} 个预算窗口", pause.window)),
            ..Self::default()
        };
        let mut files: Vec<qcode_core::event::DiffFileStat> = Vec::new();
        let mut commands: Vec<PauseCommand> = Vec::new();
        let mut failures: Vec<String> = Vec::new();
        for block in turn
            .process
            .iter()
            .filter_map(|index| app.blocks.get(*index))
        {
            let Block::Tool(entry) = block else {
                continue;
            };
            if entry.child {
                continue;
            }
            if let Some(diff) = &entry.diff {
                merge_diff(&mut files, diff);
            }
            if let Some(command) = entry
                .arguments
                .get("command")
                .and_then(|value| value.as_str())
                .filter(|_| entry.name == "bash")
            {
                let failed = entry.status == Some(ToolStatus::Error);
                if !commands.iter().any(|seen| seen.command == command) {
                    commands.push(PauseCommand {
                        command: truncate(command, DETAIL_LIMIT),
                        failed,
                    });
                }
            }
            if entry.status == Some(ToolStatus::Error) {
                failures.push(failure_line(entry));
            }
        }
        files.sort_by(|left, right| left.path.cmp(&right.path));
        if !files.is_empty() {
            let additions: u32 = files.iter().map(|file| file.additions).sum();
            let deletions: u32 = files.iter().map(|file| file.deletions).sum();
            card.files = Some(format!(
                "修改了 {} 个文件（+{additions} −{deletions}）",
                files.len()
            ));
        }
        card.more_commands = commands.len().saturating_sub(COMMAND_LIMIT);
        commands.truncate(COMMAND_LIMIT);
        card.commands = commands;
        failures.truncate(FAILURE_LIMIT);
        card.failures = failures;
        Some(card)
    }
}

/// Same path in several calls counts once, with the counts summed: a file
/// edited three times is one file, not three.
fn merge_diff(files: &mut Vec<qcode_core::event::DiffFileStat>, diff: &ToolDiff) {
    for stat in &diff.files {
        match files.iter_mut().find(|seen| seen.path == stat.path) {
            Some(seen) => {
                seen.additions += stat.additions;
                seen.deletions += stat.deletions;
                seen.bytes = stat.bytes;
                seen.kind = stat.kind;
            }
            None => files.push(stat.clone()),
        }
    }
}

fn failure_line(entry: &ToolEntry) -> String {
    let detail = entry
        .output
        .lines()
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("no output");
    format!(
        "{}: {} — {}",
        entry.name,
        if entry.summary.is_empty() {
            "failed"
        } else {
            entry.summary.as_str()
        },
        truncate(detail, DETAIL_LIMIT)
    )
}

fn headline(pause: &PauseInfo) -> String {
    match pause.limit {
        qcode_core::event::PauseLimit::Turns(limit) => {
            format!("已用 {}/{} 轮", pause.used.turns, limit)
        }
        qcode_core::event::PauseLimit::Usd(limit) => {
            format!(
                "成本已达预算 ${limit:.2} 的 95%（已用 ${:.2}）",
                pause.used.cost_usd
            )
        }
    }
}

/// Char-safe cut: a byte slice would panic on Chinese output.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    format!("{}…", text.chars().take(limit).collect::<String>())
}

#[cfg(test)]
mod tests {
    use super::*;
    use qcode_core::event::{
        DiffKind, Event, EventKind, PauseLimit, PauseReason, PauseUsage, ToolCall, ToolResult,
        ToolStatus, TurnEnded, TurnStarted, TurnStatus, Usage,
    };

    fn paused_app(events: Vec<EventKind>) -> App {
        let mut app = App::new("mock", "m");
        // The engine opens the turn, then writes the user message into it.
        let mut all = vec![
            EventKind::TurnStarted(TurnStarted {
                input: "do it".to_string(),
            }),
            EventKind::UserMessage(qcode_core::event::UserMessage {
                text: "do it".to_string(),
                attachments: Vec::new(),
                queued_id: None,
                starts_segment: false,
                continuation: None,
                question_reply: None,
                scheduled: None,
            }),
        ];
        all.extend(events);
        // Sequences are what the reducer uses to name turns and blocks.
        let events: Vec<Event> = all
            .into_iter()
            .enumerate()
            .map(|(index, kind)| Event::new(index as u64 + 1, "s1", 0, kind))
            .collect();
        for event in &events {
            app.apply(event);
        }
        app
    }

    #[test]
    fn counts_files_commands_and_failures() {
        let app = paused_app(vec![
            EventKind::ToolCall(ToolCall {
                call_id: "c1".to_string(),
                name: "write".to_string(),
                arguments: serde_json::json!({"path": "a.rs"}),
            }),
            EventKind::ToolResult(ToolResult {
                call_id: "c1".to_string(),
                status: ToolStatus::Ok,
                output: "ok".to_string(),
                metadata: Default::default(),
                diff: Some(ToolDiff {
                    patch: String::new(),
                    files: vec![qcode_core::event::DiffFileStat {
                        path: "a.rs".to_string(),
                        kind: DiffKind::Modified,
                        additions: 10,
                        deletions: 2,
                        bytes: 100,
                    }],
                    truncated: false,
                }),
            }),
            EventKind::ToolCall(ToolCall {
                call_id: "c2".to_string(),
                name: "write".to_string(),
                arguments: serde_json::json!({"path": "a.rs"}),
            }),
            EventKind::ToolResult(ToolResult {
                call_id: "c2".to_string(),
                status: ToolStatus::Ok,
                output: "ok".to_string(),
                metadata: Default::default(),
                diff: Some(ToolDiff {
                    patch: String::new(),
                    files: vec![qcode_core::event::DiffFileStat {
                        path: "a.rs".to_string(),
                        kind: DiffKind::Modified,
                        additions: 5,
                        deletions: 1,
                        bytes: 120,
                    }],
                    truncated: false,
                }),
            }),
            EventKind::ToolCall(ToolCall {
                call_id: "c3".to_string(),
                name: "bash".to_string(),
                arguments: serde_json::json!({"command": "cargo test"}),
            }),
            EventKind::ToolResult(ToolResult {
                call_id: "c3".to_string(),
                status: ToolStatus::Error,
                output: "error[E0308]: mismatched types\nmore".to_string(),
                metadata: Default::default(),
                diff: None,
            }),
            EventKind::TurnEnded(TurnEnded {
                status: TurnStatus::Paused,
                usage: Usage::default(),
                cost_usd: 0.0,
                pause: Some(PauseInfo {
                    reason: PauseReason::MaxTurns,
                    limit: PauseLimit::Turns(64),
                    used: PauseUsage {
                        turns: 64,
                        cost_usd: 0.0,
                    },
                    window: 2,
                    summary: Some("done: a\nleft: b".to_string()),
                }),
            }),
        ]);
        let card = PauseCard::of(&app, 0).expect("paused turn");
        assert_eq!(card.headline, "已用 64/64 轮");
        assert_eq!(card.window_note.as_deref(), Some("第 2 个预算窗口"));
        // Same path twice is one file, counts summed.
        assert_eq!(card.files.as_deref(), Some("修改了 1 个文件（+15 −3）"));
        assert_eq!(card.commands.len(), 1);
        assert!(card.commands[0].failed);
        assert_eq!(card.failures.len(), 1);
        assert!(card.failures[0].contains("error[E0308]"));
        assert_eq!(card.summary.as_deref(), Some("done: a\nleft: b"));
    }

    #[test]
    fn no_card_for_a_completed_turn() {
        let app = paused_app(vec![EventKind::TurnEnded(TurnEnded {
            status: TurnStatus::Completed,
            usage: Usage::default(),
            cost_usd: 0.0,
            pause: None,
        })]);
        assert!(PauseCard::of(&app, 0).is_none());
    }

    #[test]
    fn truncation_is_char_safe() {
        let long = "修".repeat(200);
        let cut = truncate(&long, 10);
        assert_eq!(cut.chars().count(), 11);
    }
}
