//! Session title derivation and model-backed generation.
//!
//! The title pipeline (会话标题自动生成设计):
//!
//! 1. **User rename** — a `task_renamed` event, written by the user. It pins
//!    the title: automatic generation never overwrites it.
//! 2. **Generated** — a `title_generated` event, written by this module after
//!    a model call (or the deterministic fallback). Latest wins, like a rename.
//! 3. **Fallback** — the first eligible user message, truncated to a word and
//!    byte budget, derived at display time. This is the behaviour the session
//!    list and the sidebar had before generation existed.
//!
//! The pure functions here are shared by the CLI, the GUI and the generator so
//! every surface derives the same title from the same log.

use std::time::Duration;

use crate::event::{Event, EventKind, TitleSource, UserMessage};
use crate::provider::{CompletionRequest, Message, Role, StopReason};
use crate::store::SessionStore;
use crate::task::strip_schedule_prefill;
use tokio_util::sync::CancellationToken;

/// Title-length policy shared by every derivation path (设计 §6).
#[derive(Debug, Clone)]
pub struct TitleConfig {
    /// Hard UTF-8 byte cap for any accepted title.
    pub max_title_bytes: usize,
    /// Word cap (whitespace-delimited) for the deterministic fallback.
    pub fallback_max_words: usize,
    /// Byte cap for the deterministic fallback.
    pub fallback_max_bytes: usize,
    /// Byte cap for the framed model input; beyond it, generation is skipped.
    pub max_input_bytes: usize,
    /// Output-token cap for the title call.
    pub max_output_tokens: u64,
    /// End-to-end generation deadline.
    pub timeout: Duration,
    /// Soft target word count for non-CJK titles (prompt-only, not enforced).
    pub target_words: u32,
    /// Soft target character count for CJK titles (prompt-only, not enforced).
    pub target_cjk_chars: u32,
    /// Explicit route override; must pair with `model`.
    pub provider: Option<String>,
    pub model: Option<String>,
}

impl Default for TitleConfig {
    fn default() -> Self {
        Self {
            max_title_bytes: 80,
            fallback_max_words: 5,
            fallback_max_bytes: 40,
            max_input_bytes: 2000,
            max_output_tokens: 64,
            timeout: Duration::from_secs(15),
            target_words: 5,
            target_cjk_chars: 12,
            provider: None,
            model: None,
        }
    }
}

/// Control and invisible-directional characters that must never reach a title.
const CONTROL_CHARACTERS: &str = "\u{0000}\u{0001}\u{0002}\u{0003}\u{0004}\u{0005}\u{0006}\u{0007}\u{0008}\u{000B}\u{000C}\u{000E}\u{000F}\u{0010}\u{0011}\u{0012}\u{0013}\u{0014}\u{0015}\u{0016}\u{0017}\u{0018}\u{0019}\u{001A}\u{001B}\u{001C}\u{001D}\u{001E}\u{001F}\u{007F}\u{0080}\u{0081}\u{0082}\u{0083}\u{0084}\u{0085}\u{0086}\u{0087}\u{0088}\u{0089}\u{008A}\u{008B}\u{008C}\u{008D}\u{008E}\u{008F}\u{0090}\u{0091}\u{0092}\u{0093}\u{0094}\u{0095}\u{0096}\u{0097}\u{0098}\u{0099}\u{009A}\u{009B}\u{009C}\u{009D}\u{009E}\u{009F}";
const INVISIBLE_DIRECTIONAL: &str = "\u{200B}\u{200E}\u{200F}\u{202A}\u{202B}\u{202C}\u{202D}\u{202E}\u{2060}\u{2061}\u{2062}\u{2063}\u{2064}\u{2066}\u{2067}\u{2068}\u{2069}\u{206A}\u{206B}\u{206C}\u{206D}\u{206E}\u{206F}\u{FEFF}";

/// Strip terminal escapes, control and invisible-directional characters, fold
/// whitespace to single spaces and trim. The one cleaning pass every title
/// text goes through, so a crafted message cannot hide control codes behind a
/// displayed title.
fn clean_title_text(input: &str) -> String {
    let mut cleaned = String::with_capacity(input.len());
    // 0 = text, 1 = saw ESC, 2 = in CSI, 3 = in OSC, 4 = saw ST's ESC byte.
    let mut state = 0u8;
    let mut previous_space = true;
    for character in input.chars() {
        match state {
            // In an OSC sequence, everything is swallowed until BEL or ST
            // (ESC \). The ST's ESC byte moves us to state 4, which consumes
            // the following backslash.
            3 => {
                if character == '\u{0007}' {
                    state = 0;
                } else if character == '\u{001B}' {
                    state = 4;
                }
                continue;
            }
            4 => {
                state = 0;
                if character == '\\' {
                    continue;
                }
            }
            // In a CSI sequence, everything is swallowed until the final byte
            // (@–~) that closes the parameter run.
            2 => {
                if ('\u{0040}'..='\u{007E}').contains(&character) {
                    state = 0;
                }
                continue;
            }
            // After ESC: `[` opens CSI, `]` opens OSC, anything else was a
            // two-byte escape and the byte itself is dropped.
            1 => {
                state = 0;
                match character {
                    '[' => {
                        state = 2;
                        continue;
                    }
                    ']' => {
                        state = 3;
                        continue;
                    }
                    _ => {}
                }
            }
            _ => {}
        }
        match character {
            '\u{001B}' => state = 1,
            '\u{009B}' => state = 2,
            character if CONTROL_CHARACTERS.contains(character) => {}
            character if INVISIBLE_DIRECTIONAL.contains(character) => {}
            character if character.is_whitespace() => {
                if !previous_space {
                    cleaned.push(' ');
                }
                previous_space = true;
            }
            character => {
                cleaned.push(character);
                previous_space = false;
            }
        }
    }
    cleaned.trim().to_string()
}

/// Truncate to a UTF-8 byte budget without splitting a code point.
fn truncate_utf8(input: &str, max_bytes: usize) -> String {
    if input.len() <= max_bytes {
        return input.to_string();
    }
    let mut used = 0usize;
    let mut output = String::new();
    for character in input.chars() {
        let bytes = character.len_utf8();
        if used + bytes > max_bytes {
            break;
        }
        output.push(character);
        used += bytes;
    }
    output
}

/// Clean a title and enforce its byte budget. `max_bytes` 0 means unlimited.
pub fn normalize_title(input: &str, max_bytes: usize) -> String {
    let cleaned = clean_title_text(input);
    if max_bytes == 0 {
        cleaned
    } else {
        truncate_utf8(&cleaned, max_bytes)
    }
}

/// Deterministic first-message fallback: the leading whitespace-delimited
/// words, byte-truncated. Both limits apply, so a CJK message that is one long
/// "word" still gets a bounded title.
pub fn fallback_title(input: &str, max_words: usize, max_bytes: usize) -> String {
    let cleaned = clean_title_text(input);
    let words: Vec<&str> = cleaned
        .split(' ')
        .filter(|word| !word.is_empty())
        .take(max_words)
        .collect();
    truncate_utf8(&words.join(" "), max_bytes)
}

/// Asterisk emphasis wrapping a whole line, such as `**标题**`.
fn emphasis_wrapped(line: &str) -> Option<&str> {
    let marker = line
        .chars()
        .take(3)
        .take_while(|character| *character == '*')
        .count();
    if marker == 0 || line.len() < marker * 2 {
        return None;
    }
    // `*` is single-byte ASCII, so the char counts double as byte offsets.
    let inner = &line[marker..line.len() - marker];
    if inner.is_empty() || inner.chars().any(|character| character == '*') {
        return None;
    }
    Some(inner)
}

/// Take the candidate from model output: the first non-empty line, without an
/// asterisk emphasis that wraps that whole line. A model that ignores the
/// one-line instruction usually puts the title first and commentary after it.
pub fn title_from_model_output(text: &str) -> String {
    let line = text
        .split('\n')
        .map(str::trim)
        .find(|line| !line.is_empty())
        .unwrap_or("")
        .to_string();
    match emphasis_wrapped(&line) {
        Some(inner) => inner.to_string(),
        None => line,
    }
}

/// JSON-framed input so message text cannot break the request boundary.
pub fn frame_title_input(text: &str) -> String {
    format!(
        "从这份人类消息生成会话标题（JSON）：{}",
        serde_json::to_string(text).unwrap_or_default()
    )
}

/// The system instruction for one title call. Self-authored, no fixed wording
/// borrowed from anywhere; the model owns the language choice.
pub fn title_system_prompt(target_words: u32, target_cjk_chars: u32) -> String {
    format!(
        "为一个编程辅助会话生成简洁标题。要求：\n\
         - 只输出一行纯文本标题，不要引号、前缀、解释、Markdown 或终端控制码；\n\
         - 使用消息所用的语言；\n\
         - 非中日韩语言大约 {target_words} 个单词，中日韩语言大约 {target_cjk_chars} 个字；\n\
         - 若消息信息量很少，仍返回一个简短可行的标题（例如「问候」），不要解释。"
    )
}

/// Whether a user message is eligible input for a session title: typed by the
/// user, not an engine injection or a scheduled delivery, and non-empty after
/// cleaning.
pub fn is_eligible_message(message: &UserMessage) -> bool {
    if message.continuation.is_some()
        || message.scheduled.is_some()
        || message.question_reply.is_some()
        || message.queued_id.is_some()
        || message.starts_segment
    {
        return false;
    }
    !clean_title_text(&strip_schedule_prefill(&message.text)).is_empty()
}

/// The first eligible user message: its seq and its cleaned text.
pub fn first_eligible_message(events: &[Event]) -> Option<(u64, String)> {
    for event in events {
        if let EventKind::UserMessage(message) = &event.kind {
            if !is_eligible_message(message) {
                continue;
            }
            let text = clean_title_text(&strip_schedule_prefill(&message.text));
            if !text.is_empty() {
                return Some((event.seq, text));
            }
        }
    }
    None
}

/// The latest title event (rename or generated), last wins by seq.
pub fn latest_title(events: &[Event]) -> Option<String> {
    events.iter().rev().find_map(|event| match &event.kind {
        EventKind::TaskRenamed(renamed) => {
            let title = renamed.title.trim();
            (!title.is_empty()).then(|| title.to_string())
        }
        EventKind::TitleGenerated(generated) => {
            let title = generated.title.trim();
            (!title.is_empty()).then(|| title.to_string())
        }
        _ => None,
    })
}

/// Whether a user rename pins the session title. A pinned session never gets
/// automatic generation, and a generation never overwrites it.
pub fn is_pinned(events: &[Event]) -> bool {
    events
        .iter()
        .any(|event| matches!(&event.kind, EventKind::TaskRenamed(_)))
}

/// The title the display layer shows for a session: the latest title event,
/// then the first-message fallback.
pub fn display_title(events: &[Event], config: &TitleConfig) -> Option<String> {
    latest_title(events).or_else(|| {
        first_eligible_message(events).and_then(|(_, text)| {
            let title = fallback_title(&text, config.fallback_max_words, config.fallback_max_bytes);
            (!title.is_empty()).then_some(title)
        })
    })
}

/// Why a generation attempt produced nothing worth persisting.
#[derive(Debug)]
pub enum TitleError {
    /// The session already has a title or is pinned: no-op, not a failure.
    AlreadyTitled,
    /// No eligible user message to derive from.
    NoInput,
    /// The framed input exceeds `max_input_bytes`.
    InputTooLarge,
    /// The provider call failed (timeout, stream error, bad output, …).
    Failed(String),
}

impl std::fmt::Display for TitleError {
    fn fmt(&self, formatter: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::AlreadyTitled => formatter.write_str("session already has a title"),
            Self::NoInput => formatter.write_str("no eligible user message"),
            Self::InputTooLarge => formatter.write_str("title input exceeds the byte cap"),
            Self::Failed(message) => formatter.write_str(message),
        }
    }
}

/// Run one model title generation and append the result when the session is
/// still untitled and unpinned.
///
/// Pure log semantics: the generator reloads the log before appending, so a
/// rename that lands while the call is in flight aborts the append instead of
/// overwriting the user's choice.
pub async fn generate_title(
    provider: &dyn crate::provider::Provider,
    store: &SessionStore,
    session_id: &str,
    config: &TitleConfig,
    cancel: &CancellationToken,
    pending_input: Option<&str>,
) -> Result<Option<Event>, TitleError> {
    let events = store
        .load(session_id)
        .map_err(|error| TitleError::Failed(error.to_string()))?;
    if is_pinned(&events) || latest_title(&events).is_some() {
        return Ok(None);
    }
    // 生成来源：日志里的首条合格消息；回合刚开始、消息还没写入时用触发者
    // 传入的 pending_input（与回合并行生成，对齐「标题在回合中就已生成」）。
    let source = first_eligible_message(&events).or_else(|| {
        pending_input.and_then(|text| {
            let text = clean_title_text(&strip_schedule_prefill(text));
            (!text.is_empty()).then(|| (0, text))
        })
    });
    let Some((_, first_text)) = source else {
        return Ok(None);
    };

    // Route: an explicit override, or the session's own provider/model.
    let (provider_name, model) =
        if let (Some(provider), Some(model)) = (&config.provider, &config.model) {
            (provider.clone(), model.clone())
        } else {
            let started = events.iter().find_map(|event| match &event.kind {
                EventKind::SessionStarted(started) => Some(started),
                _ => None,
            });
            match started {
                Some(started) => (provider.name().to_string(), started.model.clone()),
                None => return Err(TitleError::NoInput),
            }
        };

    let generated = run_title_call(provider, config, cancel, &first_text, &model).await;

    // Re-check before appending: a rename (or a completed generation from a
    // racing turn) that landed in between must win.
    let events = store
        .load(session_id)
        .map_err(|error| TitleError::Failed(error.to_string()))?;
    if is_pinned(&events) || latest_title(&events).is_some() {
        return Ok(None);
    }
    // 落盘前从日志解析首条消息的真实 seq——pending 分支触发时消息尚未写入。
    let message_seqs = first_eligible_message(&events)
        .map(|(seq, _)| vec![seq])
        .unwrap_or_default();

    match generated {
        Ok(title) => {
            let event = append_title_waiting_lock(
                store,
                session_id,
                title,
                message_seqs,
                TitleSource::Auto {
                    provider: provider_name,
                    model,
                },
                cancel,
            )
            .await?;
            Ok(Some(event))
        }
        // 静默回退（设计 §9）：把确定性兜底写成事件——既给出标题，也把这次
        // 尝试记进日志，后续 turn 看到已有标题便不再调度（设计 §5 不重试）。
        Err(_) => {
            let fallback = fallback_title(
                &first_text,
                config.fallback_max_words,
                config.fallback_max_bytes,
            );
            if fallback.is_empty() {
                return Ok(None);
            }
            let event = append_title_waiting_lock(
                store,
                session_id,
                fallback,
                message_seqs,
                TitleSource::Fallback,
                cancel,
            )
            .await?;
            Ok(Some(event))
        }
    }
}

/// 落盘标题事件，会话锁被占用（回合正在写日志）时等待重试——回合结束必然
/// 释放锁，因此标题生成可与回合并行、回合结束立即落盘。
async fn append_title_waiting_lock(
    store: &SessionStore,
    session_id: &str,
    title: String,
    message_seqs: Vec<u64>,
    source: TitleSource,
    cancel: &CancellationToken,
) -> Result<Event, TitleError> {
    loop {
        match store.append_title_generated(
            session_id,
            title.clone(),
            message_seqs.clone(),
            source.clone(),
        ) {
            Ok(event) => return Ok(event),
            Err(crate::store::StoreError::Locked(_)) => {
                if cancel.is_cancelled() {
                    return Err(TitleError::Failed(
                        "title generation cancelled while waiting for the session lock".to_string(),
                    ));
                }
                tokio::time::sleep(std::time::Duration::from_millis(200)).await;
            }
            Err(error) => return Err(TitleError::Failed(error.to_string())),
        }
    }
}

/// One model title call: framed input, byte cap, deadline, stream, parsing.
/// `model` names the route the provider will record.
async fn run_title_call(
    provider: &dyn crate::provider::Provider,
    config: &TitleConfig,
    cancel: &CancellationToken,
    first_text: &str,
    model: &str,
) -> Result<String, TitleError> {
    let input = frame_title_input(first_text);
    if input.len() > config.max_input_bytes {
        return Err(TitleError::InputTooLarge);
    }

    let request = CompletionRequest {
        model: model.to_string(),
        system: title_system_prompt(config.target_words, config.target_cjk_chars),
        messages: vec![Message {
            role: Role::User,
            blocks: vec![crate::event::ContentBlock::Text { text: input }],
            tool_call_id: None,
        }],
        tools: Vec::new(),
        max_output_tokens: Some(config.max_output_tokens),
    };

    let collected = tokio::time::timeout(config.timeout, async {
        crate::engine::collect_stream(provider, request, cancel, None).await
    })
    .await
    .map_err(|_| TitleError::Failed("title generation timed out".to_string()))?
    .map_err(|error| TitleError::Failed(error.to_string()))?;

    if collected.cancelled {
        return Err(TitleError::Failed("title generation cancelled".to_string()));
    }
    if collected.stop_reason != StopReason::EndTurn {
        return Err(TitleError::Failed(format!(
            "title output stopped early ({:?})",
            collected.stop_reason
        )));
    }
    let text: String = collected
        .blocks
        .iter()
        .filter_map(|block| match block {
            crate::event::ContentBlock::Text { text } => Some(text.as_str()),
            _ => None,
        })
        .collect::<Vec<_>>()
        .join(" ");
    let title = normalize_title(&title_from_model_output(&text), config.max_title_bytes);
    if title.is_empty() {
        return Err(TitleError::Failed(
            "title model produced no text".to_string(),
        ));
    }
    Ok(title)
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::event::{Event, EventKind, TaskRenamed, TitleGenerated, TitleSource};

    fn user_event(seq: u64, text: &str) -> Event {
        Event::new(
            seq,
            "s",
            1,
            EventKind::UserMessage(UserMessage {
                starts_segment: false,
                queued_id: None,
                text: text.to_string(),
                attachments: vec![],
                continuation: None,
                question_reply: None,
                scheduled: None,
            }),
        )
    }

    fn renamed_event(seq: u64, title: &str) -> Event {
        Event::new(
            seq,
            "s",
            1,
            EventKind::TaskRenamed(TaskRenamed {
                title: title.to_string(),
            }),
        )
    }

    fn generated_event(seq: u64, title: &str) -> Event {
        Event::new(
            seq,
            "s",
            1,
            EventKind::TitleGenerated(TitleGenerated {
                title: title.to_string(),
                message_seqs: vec![1],
                source: TitleSource::Auto {
                    provider: "mock".to_string(),
                    model: "mock-1".to_string(),
                },
            }),
        )
    }

    #[test]
    fn clean_removes_escapes_and_controls_and_folds_whitespace() {
        assert_eq!(
            clean_title_text("  a\u{1B}]0;window\u{0007}b  \n c\t d  "),
            "ab c d"
        );
        assert_eq!(clean_title_text("x\u{200B}\u{202E}y"), "xy");
        assert_eq!(clean_title_text("\u{1B}[31mred\u{1B}[0m"), "red");
        assert_eq!(clean_title_text("a\u{1B}]0;title\u{001B}\\b"), "ab");
    }

    #[test]
    fn truncate_does_not_split_a_code_point() {
        assert_eq!(truncate_utf8("中文标题", 2), "");
        assert_eq!(truncate_utf8("中文标题", 3), "中");
        assert_eq!(truncate_utf8("中文标题", 15), "中文标题");
    }

    #[test]
    fn fallback_caps_words_then_bytes() {
        assert_eq!(
            fallback_title("one two three four five six", 5, 40),
            "one two three four five"
        );
        assert_eq!(fallback_title("中文 英文 mix", 2, 6), "中文");
    }

    #[test]
    fn model_output_takes_first_line_and_strips_emphasis() {
        assert_eq!(
            title_from_model_output("**修复登录 Bug**\n一些解释"),
            "修复登录 Bug"
        );
        assert_eq!(title_from_model_output("plain title\nmore"), "plain title");
    }

    #[test]
    fn latest_title_prefers_the_newest_event() {
        let events = [generated_event(1, "auto"), renamed_event(2, "用户改名")];
        assert_eq!(latest_title(&events).as_deref(), Some("用户改名"));
        let events = [renamed_event(1, "改名"), generated_event(2, "新标题")];
        assert_eq!(latest_title(&events).as_deref(), Some("新标题"));
    }

    #[test]
    fn display_title_uses_event_then_fallback() {
        let config = TitleConfig::default();
        assert_eq!(
            display_title(&[user_event(1, "帮我看看这个 bug")], &config).as_deref(),
            Some("帮我看看这个 bug")
        );
        assert_eq!(
            display_title(
                &[user_event(1, "a"), generated_event(2, "模型标题")],
                &config
            )
            .as_deref(),
            Some("模型标题")
        );
        assert_eq!(display_title(&[], &config), None);
    }

    #[test]
    fn eligible_messages_exclude_injections() {
        let mut scheduled = user_event(1, "daily report");
        let EventKind::UserMessage(message) = &mut scheduled.kind else {
            unreachable!()
        };
        message.scheduled = Some(crate::schedule::ScheduledMessage {
            task_id: "t".to_string(),
            title: "t".to_string(),
            prompt: "p".to_string(),
            at: "2026-11-16T08:00:00".to_string(),
        });
        assert!(!is_eligible_message(message));

        let mut steering = user_event(1, "steer");
        let EventKind::UserMessage(message) = &mut steering.kind else {
            unreachable!()
        };
        message.starts_segment = true;
        assert!(!is_eligible_message(message));
    }
}
