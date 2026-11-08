//! What a paused turn actually got done, derived from the event log.
//!
//! The hand-off summary the model writes ([`crate::event::PauseInfo::summary`])
//! says what is *left*; this says what *happened*, and unlike the summary it is
//! free and cannot be wrong. Everything here is recomputed from the JSONL log,
//! so a replayed session shows the same report it showed live (ADR-0060 §2.4).

use serde::{Deserialize, Serialize};

use crate::event::{DiffFileStat, DiffKind, Event, EventKind, ToolStatus};

/// One command the turn ran, for the "what did it do" list.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct CommandLine {
    pub command: String,
    #[serde(default, skip_serializing_if = "Option::is_none")]
    pub exit_code: Option<i32>,
    pub outcome: ToolStatus,
}

/// A tool call that did not succeed. Worth surfacing on a pause: it is often
/// the reason the task is unfinished.
#[derive(Debug, Clone, PartialEq, Serialize, Deserialize)]
pub struct FailureNote {
    pub tool: String,
    pub status: ToolStatus,
    /// First line of the output, truncated.
    pub detail: String,
}

/// Aggregate of one turn's tool work.
#[derive(Debug, Clone, Default, PartialEq, Serialize, Deserialize)]
pub struct ProgressReport {
    pub tool_calls: u32,
    /// Files touched by successful write-ish calls, one entry per path with the
    /// counts summed across calls.
    pub files: Vec<DiffFileStat>,
    pub commands: Vec<CommandLine>,
    pub failures: Vec<FailureNote>,
    /// Last assistant text of the turn, truncated to [`LAST_TEXT_LIMIT`] chars.
    pub last_text: String,
}

/// How much of the final answer the report keeps. The full text is already in
/// the transcript; this is a reminder of where the turn stopped.
pub const LAST_TEXT_LIMIT: usize = 600;

/// How much of a failing tool's output one note keeps.
const DETAIL_LIMIT: usize = 160;

impl ProgressReport {
    /// True when the turn did nothing observable (no tools, no text) — the
    /// caller then shows "no tool calls this turn" instead of an empty list.
    pub fn is_empty(&self) -> bool {
        self.tool_calls == 0 && self.last_text.is_empty()
    }

    /// `修改了 3 个文件（+120 −8）` style one-liner, or `None` when nothing
    /// was written.
    pub fn files_summary(&self) -> Option<String> {
        if self.files.is_empty() {
            return None;
        }
        let additions: u32 = self.files.iter().map(|file| file.additions).sum();
        let deletions: u32 = self.files.iter().map(|file| file.deletions).sum();
        Some(format!(
            "修改了 {} 个文件（+{additions} −{deletions}）",
            self.files.len()
        ))
    }
}

/// Build the report for one turn of `events`.
///
/// `turn` is the event-level turn number; `None` reports on every turn in the
/// log, which is what `qc cost --session` style summaries want.
pub fn report(events: &[Event], turn: Option<u32>) -> ProgressReport {
    let mut report = ProgressReport::default();
    let mut names: std::collections::HashMap<&str, &str> = std::collections::HashMap::new();
    let mut lines: std::collections::HashMap<&str, &serde_json::Value> =
        std::collections::HashMap::new();
    for event in events {
        if turn.is_some_and(|turn| event.turn != turn) {
            continue;
        }
        match &event.kind {
            EventKind::ToolCall(call) => {
                report.tool_calls += 1;
                names.insert(call.call_id.as_str(), call.name.as_str());
                lines.insert(call.call_id.as_str(), &call.arguments);
            }
            EventKind::ToolResult(result) => {
                let name = names.get(result.call_id.as_str()).copied().unwrap_or("");
                if result.status == ToolStatus::Ok {
                    if let Some(diff) = &result.diff {
                        merge_files(&mut report.files, &diff.files);
                    }
                    if name == "run_shell"
                        && let Some(command) = shell_command(
                            lines
                                .get(result.call_id.as_str())
                                .copied()
                                .unwrap_or(&serde_json::Value::Null),
                        )
                    {
                        report.commands.push(CommandLine {
                            command,
                            exit_code: result.metadata.exit_code,
                            outcome: result.status,
                        });
                    }
                } else {
                    report.failures.push(FailureNote {
                        tool: name.to_string(),
                        status: result.status,
                        detail: first_line(&result.output),
                    });
                }
            }
            EventKind::AssistantMessage(message) => {
                let text = message
                    .blocks
                    .iter()
                    .filter_map(|block| match block {
                        crate::event::ContentBlock::Text { text } => Some(text.as_str()),
                        _ => None,
                    })
                    .collect::<String>();
                if !text.trim().is_empty() {
                    report.last_text = truncate(text.trim(), LAST_TEXT_LIMIT);
                }
            }
            _ => {}
        }
    }
    report
}

/// Fold one call's per-file counts into the turn total, summing a path that was
/// touched several times (`edit_file` twice) instead of listing it twice.
fn merge_files(files: &mut Vec<DiffFileStat>, touched: &[DiffFileStat]) {
    for file in touched {
        match files.iter_mut().find(|known| known.path == file.path) {
            Some(known) => {
                known.additions += file.additions;
                known.deletions += file.deletions;
                known.bytes = file.bytes;
                // A file created and then modified is still a creation for the
                // report's purpose; a create followed by a delete nets out.
                known.kind = match (known.kind, file.kind) {
                    (DiffKind::Created, DiffKind::Deleted) => DiffKind::Deleted,
                    (_, later) => later,
                };
            }
            None => files.push(file.clone()),
        }
    }
}

/// The command line of a `run_shell` call, for the report.
fn shell_command(arguments: &serde_json::Value) -> Option<String> {
    arguments
        .get("command")
        .and_then(|value| value.as_str())
        .map(str::to_string)
}

fn first_line(output: &str) -> String {
    let line = output.lines().next().unwrap_or("").trim();
    truncate(line, DETAIL_LIMIT)
}

/// Cut `text` to `limit` **characters** (not bytes) with an ellipsis.
fn truncate(text: &str, limit: usize) -> String {
    if text.chars().count() <= limit {
        return text.to_string();
    }
    let mut out: String = text.chars().take(limit).collect();
    out.push('…');
    out
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{ContentBlock, ToolCall, ToolDiff, ToolMetadata, ToolResult};
    use serde_json::json;

    fn event(seq: u64, turn: u32, kind: EventKind) -> Event {
        Event::new(seq, "s1", turn, kind)
    }

    fn diff(path: &str, kind: DiffKind, additions: u32, deletions: u32) -> ToolDiff {
        ToolDiff {
            patch: String::new(),
            files: vec![DiffFileStat {
                path: path.to_string(),
                kind,
                additions,
                deletions,
                bytes: 10,
            }],
            truncated: false,
        }
    }

    fn call(seq: u64, turn: u32, call_id: &str, name: &str, arguments: serde_json::Value) -> Event {
        event(
            seq,
            turn,
            EventKind::ToolCall(ToolCall {
                call_id: call_id.to_string(),
                name: name.to_string(),
                arguments,
            }),
        )
    }

    fn result(seq: u64, turn: u32, call_id: &str, status: ToolStatus, output: &str) -> Event {
        event(
            seq,
            turn,
            EventKind::ToolResult(ToolResult {
                call_id: call_id.to_string(),
                status,
                output: output.to_string(),
                metadata: ToolMetadata::default(),
                diff: None,
            }),
        )
    }

    #[test]
    fn a_paused_turn_reports_what_it_changed() {
        let mut write = result(3, 1, "a", ToolStatus::Ok, "ok");
        if let EventKind::ToolResult(result) = &mut write.kind {
            result.diff = Some(diff("src/lib.rs", DiffKind::Modified, 12, 3));
        }
        let mut second = result(5, 1, "b", ToolStatus::Ok, "ok");
        if let EventKind::ToolResult(result) = &mut second.kind {
            result.diff = Some(diff("src/lib.rs", DiffKind::Modified, 4, 1));
        }
        let events = vec![
            call(1, 1, "a", "edit_file", json!({})),
            event(
                2,
                1,
                EventKind::AssistantMessage(crate::event::AssistantMessage {
                    blocks: vec![ContentBlock::Text {
                        text: "half way".into(),
                    }],
                    provider_raw: json!({}),
                    usage: Default::default(),
                }),
            ),
            write,
            call(4, 1, "b", "write_file", json!({})),
            second,
        ];

        let report = report(&events, Some(1));
        assert_eq!(report.tool_calls, 2);
        assert_eq!(report.files.len(), 1, "same path folded into one row");
        assert_eq!(report.files[0].additions, 16);
        assert_eq!(report.files[0].deletions, 4);
        assert_eq!(report.last_text, "half way");
        assert_eq!(
            report.files_summary().as_deref(),
            Some("修改了 1 个文件（+16 −4）")
        );
    }

    #[test]
    fn commands_and_failures_are_kept_apart() {
        let events = vec![
            call(1, 1, "a", "run_shell", json!({ "command": "cargo test" })),
            result(2, 1, "a", ToolStatus::Ok, "all good"),
            call(3, 1, "b", "read_file", json!({})),
            result(4, 1, "b", ToolStatus::Error, "no such file\nsecond line"),
        ];
        let report = report(&events, Some(1));
        assert_eq!(report.commands.len(), 1);
        assert_eq!(report.commands[0].command, "cargo test");
        assert_eq!(report.failures.len(), 1);
        assert_eq!(report.failures[0].tool, "read_file");
        assert_eq!(report.failures[0].detail, "no such file");
    }

    #[test]
    fn only_the_asked_turn_is_reported() {
        let events = vec![
            call(1, 1, "a", "read_file", json!({})),
            call(2, 2, "b", "read_file", json!({})),
        ];
        assert_eq!(report(&events, Some(2)).tool_calls, 1);
        assert_eq!(report(&events, None).tool_calls, 2);
    }

    #[test]
    fn long_text_is_cut_by_characters_not_bytes() {
        let text = "中".repeat(LAST_TEXT_LIMIT + 5);
        let cut = truncate(&text, LAST_TEXT_LIMIT);
        assert_eq!(cut.chars().count(), LAST_TEXT_LIMIT + 1);
        assert!(cut.ends_with('…'));
    }
}
